//! The commands `sandboxd` starts, and reaping.
//!
//! One thread waits for every child that exits, the orphans that a sandbox's first process
//! inherits included, and reports the exit of each started command on the stream that started
//! it. It reaps a child only while holding the lock that starting a command holds, because
//! starting may reap a child itself: the standard library waits for a child whose program
//! failed to start, and must find it unreaped.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;

use pumpkin_schema::sandbox::{ExitStatus, FromSandbox, Launch};
use stdx::Context;

use crate::session::Sender;
use crate::sys;

/// The commands started in this sandbox.
pub(crate) struct Commands {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    /// Signalled when a command starts or a started one exits, and when the reaper finds no
    /// children, which is what freezing waits for.
    changed: Condvar,
    sender: Sender,
}

struct State {
    /// The started commands still running, by process ID, each with the stream that started
    /// it. Each leads a process group of its own.
    running: HashMap<i32, u32>,
    /// How many commands have started, so the reaper knows when to look for children again.
    started: u64,
    /// Whether the reaper has found no children since the last command started.
    childless: bool,
    frozen: bool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Commands {
    /// Starts the thread that reaps children and reports exits through `sender`.
    pub(crate) fn start_reaping(sender: Sender) -> io::Result<Self> {
        let state = State {
            running: HashMap::new(),
            started: 0,
            childless: true,
            frozen: false,
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(state),
            changed: Condvar::new(),
            sender,
        });
        let reaper = Arc::clone(&shared);
        thread::Builder::new()
            .name("reaper".to_owned())
            .spawn(move || reap(&reaper))?;
        Ok(Self { shared })
    }

    /// Starts a command and replies on `stream`. The reply goes out before the command is
    /// unlocked for the reaper, so its exit can't be reported first.
    pub(crate) fn start(&self, stream: u32, launch: &Launch) -> io::Result<()> {
        let mut state = self.shared.lock();
        let started = if state.frozen {
            Err("the sandbox is frozen: start commands before freezing it".to_owned())
        } else {
            spawn(launch).map(|pid| {
                state.running.insert(pid, stream);
                state.started += 1;
                state.childless = false;
            })
        };
        self.shared.changed.notify_all();
        self.shared.sender.reply(stream, started)
    }

    /// Kills every process this sandboxd may kill, refuses further commands, and returns once
    /// no children are left. As a sandbox's first process, that means no process at all but
    /// `sandboxd`: every other one descends from it.
    pub(crate) fn freeze(&self) -> Result<(), String> {
        let mut state = self.shared.lock();
        state.frozen = true;
        kill_everything(&state).context("the sandbox's processes couldn't be killed")?;
        while !(state.running.is_empty() && state.childless) {
            state = self
                .shared
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        Ok(())
    }

    /// Kills every process this sandboxd may kill, without waiting.
    pub(crate) fn kill_all(&self) {
        // The session is over, so there is nobody left to tell about a failure.
        let _ = kill_everything(&self.shared.lock());
    }
}

/// Kills every other process in the sandbox when this is its first process, and otherwise the
/// process groups of the commands still running. A group whose leader has exited may live on
/// there, but its ID may also have been reused by now. Every group is tried, so that one that
/// can't be killed leaves no other alive, and the first failure is returned.
fn kill_everything(state: &State) -> io::Result<()> {
    if sys::is_first_process() {
        return sys::kill_all_others();
    }
    let killed = state.running.keys().map(|&group| sys::kill_group(group));
    killed.fold(Ok(()), Result::and)
}

/// Starts a command in a process group of its own, which its process leads.
fn spawn(launch: &Launch) -> Result<i32, String> {
    let Launch {
        argv,
        env,
        cwd,
        stdout,
        stderr,
    } = launch;
    let Some((program, args)) = argv.as_slice().split_first() else {
        return Err("the command is empty: it needs a program".to_owned());
    };
    let mut output = OpenOptions::new();
    output.write(true).create(true).truncate(true);
    let stdout = sys::open_regular(Path::new(stdout.as_str()), &mut output)?;
    let stderr = sys::open_regular(Path::new(stderr.as_str()), &mut output)?;
    let child = Command::new(program)
        .args(args)
        .envs(env.as_slice().iter().map(|var| (var.name(), var.value())))
        .current_dir(cwd.as_str())
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .context(format_args!("`{program}` didn't start"))?;
    // The reaper waits for it, so its handle is dropped without waiting.
    i32::try_from(child.id()).map_err(|error| error.to_string())
}

/// Reaps children forever, reporting the exits of started commands. With no children, it
/// waits until another command starts.
fn reap(shared: &Shared) {
    let mut seen = 0;
    loop {
        if let Ok(pid) = sys::wait_for_exit() {
            let mut state = shared.lock();
            let Ok(Some(status)) = sys::reap(pid) else {
                continue;
            };
            if let Some(stream) = state.running.remove(&pid) {
                let status = exit_status(status);
                // A report that can't be sent has nobody left to read it.
                let _ = shared
                    .sender
                    .send(&FromSandbox::Exited { stream, status }, &[]);
                shared.changed.notify_all();
            }
        } else {
            let mut state = shared.lock();
            if state.started == seen {
                state.childless = true;
                shared.changed.notify_all();
                while state.started == seen {
                    state = shared
                        .changed
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
            seen = state.started;
        }
    }
}

/// Decodes a status from `waitpid`, which, called without `WUNTRACED`, reports only exits and
/// deaths by signal.
fn exit_status(raw: i32) -> ExitStatus {
    let status = std::process::ExitStatus::from_raw(raw);
    match status.code() {
        Some(code) => ExitStatus::Code(code),
        None => ExitStatus::Signal(status.signal().unwrap_or_default()),
    }
}
