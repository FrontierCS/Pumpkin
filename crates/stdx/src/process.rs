//! The process control `std` lacks: commands whose caller chooses what of the environment
//! they get, a process group's kill and a bounded run ([`output`]).
//!
//! A bounded run may feed the command its standard input too ([`fed`]).
//!
//! A wait for whichever of several children exits first is [`wait_any`].

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread::{self, Scope, ScopedJoinHandle};
use std::time::Duration;
use std::{env, panic};

use rustix::io::Errno;
use rustix::process::{Pid, Signal};

use crate::Deadline;

/// The first pause between checks that a command has ended, which doubles up to [`MAX_PAUSE`].
const FIRST_PAUSE: Duration = Duration::from_millis(1);

/// The longest pause between checks that a command has ended.
// A command that starts a process of its own takes milliseconds at least, so ending up to 10
// ms after it is noise beside it, and the checks wake this thread at most 100 times a second.
const MAX_PAUSE: Duration = Duration::from_millis(10);

/// What a child gets of this process's environment.
#[derive(Clone, Copy, Debug)]
pub enum Environment {
    /// Only the variables named, those that are set.
    Only(&'static [&'static str]),
    /// All of it.
    Inherited,
}

/// A [`Command`] that runs `program` with what `environment` says of this process's
/// environment: the one way to make a [`Command`] that `clippy.toml` allows.
pub fn command(program: impl AsRef<OsStr>, environment: Environment) -> Command {
    #[expect(
        clippy::disallowed_methods,
        reason = "the constructor that `clippy.toml` names, which makes its caller state the \
                  child's environment"
    )]
    let mut command = Command::new(program);
    match environment {
        Environment::Only(names) => {
            command.env_clear();
            let set = names
                .iter()
                .filter_map(|name| Some((name, env::var_os(name)?)));
            command.envs(set);
        }
        Environment::Inherited => {}
    }
    command
}

/// The names of the variables of this process's environment that cargo sets for the package it
/// builds, runs or tests: `CARGO_MANIFEST_DIR`, `CARGO_MANIFEST_PATH` and `CARGO_PKG_*`.
///
/// A `cargo` that this process runs is to get none of them: some build scripts, such as ring's,
/// rerun when one of them changes, so a nested build with another package's would rebuild such a
/// crate, and every crate that depends on it, and the next build outside it would again.
pub fn cargo_package_vars() -> Vec<OsString> {
    env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| {
            name.to_str().is_some_and(|name| {
                name.starts_with("CARGO_PKG_")
                    || matches!(name, "CARGO_MANIFEST_DIR" | "CARGO_MANIFEST_PATH")
            })
        })
        .collect()
}

/// Runs `command` as [`Command::output`] does, with no standard input, in a process group of its
/// own, until `by`.
///
/// `None` if it hasn't ended, exited with its output closed, by then, and never started if `by`
/// has passed; at `by` its group is killed. If this process dies first, the command outlives it.
///
/// # Errors
///
/// Fails if the command or a reader can't start, if it can't be waited for or killed, or read.
pub fn output(command: &mut Command, by: Deadline) -> io::Result<Option<Output>> {
    run(command, None, by)
}

/// Runs `command` as [`output`] does, but with `input` on its standard input, which then closes,
/// though the command may read none of it.
///
/// # Errors
///
/// Fails as [`output`] does, or if `input` can't be written but for the command's closing its
/// standard input first.
pub fn fed(command: &mut Command, input: &[u8], by: Deadline) -> io::Result<Option<Output>> {
    run(command, Some(input), by)
}

/// Runs `command` as [`output`] says, with `input`, if any, on its standard input, else none.
fn run(command: &mut Command, input: Option<&[u8]>, by: Deadline) -> io::Result<Option<Output>> {
    if by.remaining().is_none() {
        return Ok(None);
    }
    let stdin = if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    let mut child = command
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A group of its own, so that the kill at `by` reaches what the command started too. The
        // cost: no signal to this process's group, such as a terminal's Ctrl-C, reaches the
        // command.
        .process_group(0)
        .spawn()?;
    let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) else {
        unreachable!(
            "the child's piped standard output or error is missing, though both are piped just \
             above, so the child has both, which `Child` holds as `Option`s all the same"
        )
    };
    let fed = child.stdin.take().zip(input);
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let status = thread::scope(|scope| {
        // Each pipe is read on a thread of its own, as `Command::output` reads both at once, so
        // that the command never waits on a full pipe; and the input is written on one too, so
        // that the command's output never waits on it.
        let readers = read_all(scope, out, &mut stdout)
            .and_then(|first| Ok([first, read_all(scope, err, &mut stderr)?]));
        let writer = readers.and_then(|readers| {
            let writer = fed
                .map(|(stdin, input)| write_all(scope, stdin, input))
                .transpose()?;
            Ok((readers, writer))
        });
        let (readers, writer) = match writer {
            Ok(threads) => threads,
            Err(error) => {
                // A thread that started ends once the command, killed, closes its pipes.
                kill(&mut child)?;
                return Err(error);
            }
        };
        let status = wait(&mut child, &readers, by);
        if !matches!(status, Ok(Some(_))) {
            kill(&mut child)?;
        }
        for thread in readers.into_iter().chain(writer) {
            thread
                .join()
                .unwrap_or_else(|panic| panic::resume_unwind(panic))?;
        }
        status
    })?;
    Ok(status.map(|status| Output {
        status,
        stdout,
        stderr,
    }))
}

/// Reads all of `pipe` into `into`, on a thread of `scope`.
fn read_all<'scope>(
    scope: &'scope Scope<'scope, '_>,
    mut pipe: impl Read + Send + 'scope,
    into: &'scope mut Vec<u8>,
) -> io::Result<ScopedJoinHandle<'scope, io::Result<()>>> {
    let spawned =
        thread::Builder::new().spawn_scoped(scope, move || pipe.read_to_end(into).map(drop));
    spawned.map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("a thread to read the command's output didn't start: {error}"),
        )
    })
}

/// Writes all of `input` to `pipe`, then closes it, on a thread of `scope`; a command that closes
/// its end first takes no more, which is no failure.
fn write_all<'scope>(
    scope: &'scope Scope<'scope, '_>,
    mut pipe: impl Write + Send + 'scope,
    input: &'scope [u8],
) -> io::Result<ScopedJoinHandle<'scope, io::Result<()>>> {
    let spawned = thread::Builder::new().spawn_scoped(scope, move || match pipe.write_all(input) {
        Err(error) if error.kind() != io::ErrorKind::BrokenPipe => Err(error),
        Ok(()) | Err(_) => Ok(()),
    });
    spawned.map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("a thread to write the command's input didn't start: {error}"),
        )
    })
}

/// Waits until `child` has exited and `readers` have read all of its output, and returns how it
/// exited: `None` if that hasn't happened by `by`.
fn wait(
    child: &mut Child,
    readers: &[ScopedJoinHandle<'_, io::Result<()>>],
    by: Deadline,
) -> io::Result<Option<ExitStatus>> {
    let mut pause = FIRST_PAUSE;
    loop {
        // Reaped only once its output has closed, so that its group, which a process it started
        // may still be in, is killed, if it must be, while `child`, which leads it, holds its ID.
        if readers.iter().all(ScopedJoinHandle::is_finished)
            && let Some(status) = child.try_wait()?
        {
            return Ok(Some(status));
        }
        let Some(left) = by.remaining() else {
            return Ok(None);
        };
        thread::sleep(pause.min(left));
        pause = (pause * 2).min(MAX_PAUSE);
    }
}

/// Waits until one of `children` has exited, and gives its index in `children` and how it
/// exited, having reaped it; or the index of one that can't be waited for, and why.
///
/// # Panics
///
/// Panics if `children` is empty, as none of them would ever exit.
pub fn wait_any(children: &mut [&mut Child]) -> (usize, io::Result<ExitStatus>) {
    assert!(!children.is_empty(), "a wait for one of no children");
    let mut pause = FIRST_PAUSE;
    loop {
        for (index, child) in children.iter_mut().enumerate() {
            match child.try_wait() {
                Ok(Some(status)) => return (index, Ok(status)),
                Ok(None) => {}
                Err(error) => return (index, Err(error)),
            }
        }
        thread::sleep(pause);
        pause = (pause * 2).min(MAX_PAUSE);
    }
}

/// Kills every process in the group that `child`, unreaped, leads, and reaps `child`.
fn kill(child: &mut Child) -> io::Result<()> {
    // `Child::id` casts the child's pid_t to the u32 it returns, which `cast_signed` undoes.
    kill_group(child.id().cast_signed())?;
    child.wait().map(drop)
}

/// Kills every process left in the process group `group`, by SIGKILL.
///
/// # Panics
///
/// Panics if `group` is 1 or less, which names no single process group.
///
/// # Errors
///
/// Fails if this process may kill none of the group's processes; finding none left is no failure.
pub fn kill_group(group: i32) -> io::Result<()> {
    // For 1, `killpg` would kill every process this one may signal, and for 0 its own group.
    let Some(pid) = Pid::from_raw(group).filter(|_| group > 1) else {
        unreachable!(
            "process group {group} is no single group: expected an ID above 1, as a caller passes \
             the group that a child of its own leads, by the child's ID, and the kernel never \
             gives a child ID 0 or 1"
        )
    };
    // SIGKILL, with no SIGTERM first: a caller that kills a group gives up on it whole, so it
    // has nothing to flush.
    match rustix::process::kill_process_group(pid, Signal::KILL) {
        Ok(()) | Err(Errno::SRCH) => Ok(()),
        Err(error) => Err(error.into()),
    }
}
