//! The commands `sandboxd` starts, and reaping.
//!
//! One thread waits for every child that exits, the orphans that a sandbox's first process
//! inherits included, and reports the exit of each started command on the stream that started
//! it. It reaps a child only while holding the lock that starting a command holds, because
//! starting may reap a child itself: the standard library waits for a child whose program
//! failed to start, and must find it unreaped.
//!
//! Whether there are children is asked of the kernel, under that lock too, by the reaper,
//! which sleeps while there are none, and by freezing, which waits while there are any. That
//! relies on children appearing only under the lock: only starting a command creates one, and
//! an orphan the kernel hands a sandbox's first process descends from a child, so it arrives
//! while there are children already.

use std::collections::HashMap;
use std::convert::Infallible;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{self, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;

use pumpkin_schema::sandbox::{ExitStatus, FromSandbox, Launch};
use stdx::{Context, unpoisoned};

use crate::session::Sender;
use crate::{print_line, sys};

/// The context of a failed [`sys::has_children`], which the reaper and freezing both call.
const HAS_CHILDREN_FAILED: &str = "finding whether sandboxd has children failed";

/// The commands started in this sandbox.
pub(crate) struct Commands {
    // Shared with the reaper thread, which reaps children while the session starts commands
    // and freezes them.
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    /// Signalled when a command starts, which the reaper waits for while there are no
    /// children, and when the reaper reaps a child, which freezing waits for while there are
    /// any.
    changed: Condvar,
    sender: Sender,
}

struct State {
    /// The started commands still running, by process ID, each with the stream that started
    /// it. Each leads a process group of its own.
    running: HashMap<i32, u32>,
    frozen: bool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        unpoisoned(self.state.lock())
    }

    /// Waits for `changed` to be signalled, releasing `state` meanwhile.
    fn wait<'a>(&self, state: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        unpoisoned(self.changed.wait(state))
    }
}

impl Commands {
    /// Starts the thread that reaps children and reports exits through `sender`. Should a wait
    /// for a child fail, the thread kills the commands and ends `sandboxd`, which can't report
    /// their exits without it.
    pub(crate) fn start_reaping(sender: Sender) -> io::Result<Self> {
        let state = State {
            running: HashMap::new(),
            frozen: false,
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(state),
            changed: Condvar::new(),
            sender,
        });
        thread::Builder::new().name("reaper".to_owned()).spawn({
            let shared = Arc::clone(&shared);
            move || {
                let Err(error) = reaper(&shared);
                // With no reaper, no exit would be reported, so `sandboxd` stops as the end of
                // the session stops it, and the gateway sees the connection close.
                let _ = kill_everything(&shared.lock());
                print_line(io::stderr(), format_args!("sandboxd: {error}"));
                process::exit(1);
            }
        })?;
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
                self.shared.changed.notify_all();
            })
        };
        self.shared.sender.reply(stream, started)
    }

    /// Kills every process this sandboxd may kill, refuses further commands, and returns once
    /// no children are left. As a sandbox's first process, that means no process at all but
    /// `sandboxd`: every other one descends from it.
    pub(crate) fn freeze(&self) -> Result<(), String> {
        let mut state = self.shared.lock();
        state.frozen = true;
        kill_everything(&state).context("the sandbox's processes couldn't be killed")?;
        // Wake the reaper: orphans of processes sandboxd didn't start (a `docker exec`)
        // become its children with no start to announce them.
        self.shared.changed.notify_all();
        // A running command is a child until the reaper reaps it, which it reports before
        // letting go of the lock, so once no children are left, every exit has been reported.
        while sys::has_children().context(HAS_CHILDREN_FAILED)? {
            state = self.shared.wait(state);
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
    let program = argv.program();
    let mut output = OpenOptions::new();
    output.write(true).create(true).truncate(true);
    let stdout = sys::open_regular(Path::new(stdout.as_str()), &mut output)?;
    let stderr = sys::open_regular(Path::new(stderr.as_str()), &mut output)?;
    let child = Command::new(program)
        .args(argv.args())
        .envs(env.as_slice().iter().map(|var| (var.name(), var.value())))
        .current_dir(cwd.as_str())
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .context(format_args!("`{program}` didn't start in {cwd}"))?;
    // The reaper waits for it, so its handle is dropped without waiting. The standard library
    // casts the child's pid_t to the u32 it returns, which `cast_signed` undoes.
    Ok(child.id().cast_signed())
}

/// Reaps children forever, reporting the exits of started commands. With no children, it
/// waits until a command starts.
///
/// A child may also stop. The kernel reports the stops of a child that made `sandboxd` its
/// tracer, as any command may with `ptrace(PTRACE_TRACEME)`, to `sandboxd`'s waits, though they
/// ask only for exits. A stopped child is still running, so the reaper reports nothing for it,
/// and [`sys::reap`] consumes the report of its stop, so that it doesn't come back. `sandboxd`
/// never resumes the child, which stays stopped until freezing or the end of the session kills
/// it. Returns only if a wait fails.
fn reaper(shared: &Shared) -> Result<Infallible, String> {
    loop {
        let mut state = shared.lock();
        while !sys::has_children().context(HAS_CHILDREN_FAILED)? {
            state = shared.wait(state);
        }
        // Commands may start while the reaper waits for a child.
        drop(state);
        #[expect(
            clippy::unreachable,
            reason = "the reaper found children under the lock, and nothing else reaps them: \
                      starting a command reaps only a child that failed to start, before it lets \
                      go of the lock"
        )]
        let Some(pid) = sys::wait_for_child().context("waiting for a child to exit failed")? else {
            unreachable!("the children found under the lock are gone, reaped by another")
        };
        let mut state = shared.lock();
        let Some(raw) = sys::reap(pid).context(format_args!("reaping child {pid} failed"))? else {
            continue;
        };
        let status = std::process::ExitStatus::from_raw(raw);
        if status.stopped_signal().is_some() {
            continue;
        }
        if let Some(stream) = state.running.remove(&pid) {
            let status = exit_status(status);
            // A report that can't be sent has nobody left to read it.
            let _ = shared
                .sender
                .send(&FromSandbox::Exited { stream, status }, &[]);
        }
        shared.changed.notify_all();
    }
}

/// Decodes the status of a child that exited or was killed by a signal.
fn exit_status(status: std::process::ExitStatus) -> ExitStatus {
    if let Some(code) = status.code() {
        return ExitStatus::Code(code);
    }
    #[expect(
        clippy::unreachable,
        reason = "waitpid reports an exit, a death by signal, a stop or a continuation; the reaper \
                  skips stops before decoding, and asks for no continuations"
    )]
    let Some(signal) = status.signal() else {
        unreachable!(
            "the status {:#x} is neither an exit nor a death by signal",
            status.into_raw()
        )
    };
    ExitStatus::Signal(signal)
}
