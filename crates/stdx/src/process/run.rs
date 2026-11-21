//! A command run to its end with its output captured, killed with its group at a deadline.

use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt as _;
use std::panic;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread::{self, Scope, ScopedJoinHandle};

use super::children::kill;
use super::{FIRST_PAUSE, MAX_PAUSE};
use crate::Deadline;

/// Runs `command` as [`Command::output`] does, with no standard input, in a process group of its
/// own, until `by`.
///
/// `None` if it hasn't ended, exited with its output closed, by then, and never started if `by`
/// has passed; at `by` its group is killed. If this process dies first, the command outlives it.
pub fn output(command: &mut Command, by: Deadline) -> io::Result<Option<Output>> {
    run(command, None, by)
}

/// Runs `command` as [`output`] does, but with `input` on its standard input, which then closes,
/// though the command may read none of it.
pub fn fed(command: &mut Command, input: &[u8], by: Deadline) -> io::Result<Option<Output>> {
    run(command, Some(input), by)
}

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

fn write_all<'scope>(
    scope: &'scope Scope<'scope, '_>,
    mut pipe: impl Write + Send + 'scope,
    input: &'scope [u8],
) -> io::Result<ScopedJoinHandle<'scope, io::Result<()>>> {
    let spawned = thread::Builder::new().spawn_scoped(scope, move || match pipe.write_all(input) {
        // A command that closes its end first takes no more, which is no failure.
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
