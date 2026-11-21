//! A command run to its end within the bound that its caller states, in a process group of its
//! own, which is killed whole at the bound, or once a captured stream passes what a run holds.

use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt as _;
use std::panic;
use std::process::{
    Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Output, Stdio,
};
use std::thread::{self, Scope, ScopedJoinHandle};
use std::time::Duration;

use super::children::kill;
use super::command::{name, shown};
use super::{FIRST_PAUSE, Failure, MAX_PAUSE};
use crate::Deadline;

// The most of each stream that a run captures, as TigerBeetle's `exec` caps each:
// https://github.com/tigerbeetle/tigerbeetle/blob/3cd7cf4799f65c81ffbc36eda788b8b26fd7da72/src/shell.zig#L468
// https://github.com/tigerbeetle/tigerbeetle/blob/3cd7cf4799f65c81ffbc36eda788b8b26fd7da72/src/shell.zig#L526-L529
const MAX_CAPTURED_BYTES: usize = 128 * 1024 * 1024;

// Every run states its bound, as each of TigerBeetle's `exec`s has a timeout, which its caller can
// change, and a child that is to outlive one goes through its `spawn`:
// https://github.com/tigerbeetle/tigerbeetle/blob/3cd7cf4799f65c81ffbc36eda788b8b26fd7da72/src/shell.zig#L462-L469
/// When [`run`] kills a command that hasn't ended.
#[derive(Clone, Copy, Debug)]
pub enum Bound {
    /// This long after it starts.
    Within(Duration),
    /// At this deadline, however late the command starts: one whose deadline has passed never
    /// starts.
    By(Deadline),
}

/// Whether [`run`] first writes the command to this process's standard error, as `$ ` and what
/// [`shown`](super::shown) shows.
#[derive(Clone, Copy, Debug)]
pub enum Echo {
    Line,
    Silent,
}

/// Where [`run`] sends the command's standard output or error.
#[derive(Clone, Copy, Debug)]
pub enum Stream {
    /// Into the [`Output`] that the run gives, up to 128 MiB, past which the command is killed.
    Captured,
    /// To this process's own.
    Inherited,
    /// To `/dev/null`.
    Discarded,
}

/// How [`run`] runs a command.
#[must_use]
#[derive(Clone, Copy, Debug)]
pub struct Run<'a> {
    bound: Bound,
    echo: Echo,
    input: Option<&'a [u8]>,
    stdout: Stream,
    stderr: Stream,
    name: Option<&'a str>,
}

impl<'a> Run<'a> {
    /// A run killed at `bound`, echoed as `echo` says, with no standard input, and both its
    /// standard output and error captured.
    pub fn new(bound: Bound, echo: Echo) -> Self {
        Self {
            bound,
            echo,
            input: None,
            stdout: Stream::Captured,
            stderr: Stream::Captured,
            name: None,
        }
    }

    /// With `input` on the command's standard input, which then closes, though the command may
    /// read none of it.
    pub fn input(self, input: &'a [u8]) -> Self {
        Self {
            input: Some(input),
            ..self
        }
    }

    pub fn stdout(self, to: Stream) -> Self {
        Self { stdout: to, ..self }
    }

    pub fn stderr(self, to: Stream) -> Self {
        Self { stderr: to, ..self }
    }

    /// Named `name` by its [`Failure`], in place of the command's program and the words after it,
    /// up to two, before the first option.
    pub fn named(self, name: &'a str) -> Self {
        Self {
            name: Some(name),
            ..self
        }
    }
}

/// Runs `command` to its end as `how` says, and gives its output, in which a stream not captured
/// is empty, if it exits with 0.
///
/// The command runs in a process group of its own, which is killed whole at its bound, or once a
/// captured stream passes 128 MiB, so what the command started ends with it; so, too, no signal to
/// this process's group, such as a terminal's Ctrl-C, reaches it. A command whose deadline has
/// passed neither starts nor echoes. If this process dies first, the command outlives it.
pub fn run(command: &mut Command, how: Run<'_>) -> Result<Output, Failure> {
    let Run {
        bound,
        echo,
        input,
        stdout,
        stderr,
        name: named,
    } = how;
    let name = named.map_or_else(|| name(command), str::to_owned);
    let by = match bound {
        Bound::Within(timeout) => Deadline::after(timeout),
        Bound::By(by) => by,
    };
    if by.remaining().is_none() {
        return Err(Failure::Unended { name, bound });
    }
    match echo {
        Echo::Line => {
            let line = format!("$ {}\n", shown(command));
            if let Err(error) = io::stderr().write_all(line.as_bytes()) {
                let error = io::Error::new(error.kind(), format!("echoing it failed: {error}"));
                return Err(Failure::Unstarted {
                    name,
                    missing: None,
                    error,
                });
            }
        }
        Echo::Silent => {}
    }
    let stdin = if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    let spawned = command
        .stdin(stdin)
        .stdout(stdio(stdout))
        .stderr(stdio(stderr))
        // A group of its own, so that the kill at the bound reaches what the command started too.
        .process_group(0)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => return Err(Failure::unstarted(command, name, error)),
    };
    match watched(&mut child, input, by) {
        Ok(Some(output)) if output.status.success() => Ok(output),
        Ok(Some(output)) => Err(Failure::Exited {
            name,
            output,
            stderr,
        }),
        Ok(None) => Err(Failure::Unended { name, bound }),
        Err(error) => Err(Failure::Unwatched { name, error }),
    }
}

fn stdio(stream: Stream) -> Stdio {
    match stream {
        Stream::Captured => Stdio::piped(),
        Stream::Inherited => Stdio::inherit(),
        Stream::Discarded => Stdio::null(),
    }
}

/// What `output`'s command printed on its standard output, as text, less one trailing newline,
/// as xshell's `read` gives it; an error if it isn't UTF-8.
// https://github.com/matklad/xshell/blob/9a405946ecbbfe50fe4a7229bd764194154e4ce6/src/lib.rs#L985-L1001
pub fn printed(output: Output) -> Result<String, String> {
    let mut printed = String::from_utf8(output.stdout).map_err(|error| {
        let error = error.utf8_error();
        format!("the standard output isn't UTF-8: {error}: expected text")
    })?;
    if printed.ends_with('\n') {
        printed.pop();
    }
    Ok(printed)
}

/// Runs `command` as [`run`] does, silently, with no standard input and both outputs captured,
/// until `by`, and gives its output however it exited.
///
/// `None` if it hasn't ended, exited with its output closed, by then, and never started if `by`
/// has passed. An error if it didn't start or couldn't be followed to its end.
pub fn output(command: &mut Command, by: Deadline) -> io::Result<Option<Output>> {
    unchecked(run(command, Run::new(Bound::By(by), Echo::Silent)))
}

/// Runs `command` as [`output`] does, but with `input` on its standard input, which then closes,
/// though the command may read none of it.
pub fn fed(command: &mut Command, input: &[u8], by: Deadline) -> io::Result<Option<Output>> {
    let how = Run::new(Bound::By(by), Echo::Silent).input(input);
    unchecked(run(command, how))
}

fn unchecked(ran: Result<Output, Failure>) -> io::Result<Option<Output>> {
    match ran {
        Ok(output) | Err(Failure::Exited { output, .. }) => Ok(Some(output)),
        Err(Failure::Unended { .. }) => Ok(None),
        Err(Failure::Unstarted { error, .. } | Failure::Unwatched { error, .. }) => Err(error),
    }
}

type Thread<'scope> = ScopedJoinHandle<'scope, io::Result<()>>;

/// Follows `child` to its end, feeding it `input` and reading each of its outputs that is piped,
/// and gives how it exited and what it wrote: `None` if it hadn't ended by `by`. Its group is
/// killed then, and whenever it can't be followed.
fn watched(child: &mut Child, input: Option<&[u8]>, by: Deadline) -> io::Result<Option<Output>> {
    let fed = child.stdin.take().zip(input);
    let (out, err) = (child.stdout.take(), child.stderr.take());
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let status = thread::scope(|scope| {
        let out = out.map(|pipe| (pipe, &mut stdout));
        let err = err.map(|pipe| (pipe, &mut stderr));
        let (mut readers, writer) = match started(scope, out, err, fed) {
            Ok(threads) => threads,
            Err(error) => {
                // A thread that started ends once the command, killed, closes its pipes.
                kill(child)?;
                return Err(error);
            }
        };
        let status = wait(child, &mut readers, by);
        if !matches!(status, Ok(Some(_))) {
            kill(child)?;
        }
        for thread in readers.into_iter().chain(writer) {
            joined(thread)?;
        }
        status
    })?;
    Ok(status.map(|status| Output {
        status,
        stdout,
        stderr,
    }))
}

/// Starts a thread to read each of `stdout` and `stderr`, and one to write `fed`'s input, so
/// that, as `Command::output` reads both at once, the command never waits on a full pipe, and its
/// output never waits on its input.
fn started<'scope>(
    scope: &'scope Scope<'scope, '_>,
    stdout: Option<(ChildStdout, &'scope mut Vec<u8>)>,
    stderr: Option<(ChildStderr, &'scope mut Vec<u8>)>,
    fed: Option<(ChildStdin, &'scope [u8])>,
) -> io::Result<(Vec<Thread<'scope>>, Option<Thread<'scope>>)> {
    let mut readers = Vec::new();
    if let Some((pipe, into)) = stdout {
        readers.push(read_all(scope, pipe, into, "output")?);
    }
    if let Some((pipe, into)) = stderr {
        readers.push(read_all(scope, pipe, into, "error")?);
    }
    let writer = fed.map(|(pipe, input)| write_all(scope, pipe, input));
    Ok((readers, writer.transpose()?))
}

/// Reads `pipe`, the command's standard `stream`, into `into`, failing once it passes
/// [`MAX_CAPTURED_BYTES`].
fn read_all<'scope>(
    scope: &'scope Scope<'scope, '_>,
    pipe: impl Read + Send + 'scope,
    into: &'scope mut Vec<u8>,
    stream: &'static str,
) -> io::Result<Thread<'scope>> {
    let read = move || {
        // A byte past the cap tells a stream that passes it from one that ends at it.
        let limit = (MAX_CAPTURED_BYTES + 1) as u64;
        pipe.take(limit).read_to_end(into)?;
        if into.len() > MAX_CAPTURED_BYTES {
            let mib = MAX_CAPTURED_BYTES >> 20;
            return Err(io::Error::other(format!(
                "its standard {stream} passed the {mib} MiB that a run captures, so it was \
                 killed: expected less"
            )));
        }
        Ok(())
    };
    let spawned = thread::Builder::new().spawn_scoped(scope, read);
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
) -> io::Result<Thread<'scope>> {
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
/// exited: `None` if that hasn't happened by `by`. A reader's error ends the wait at once.
fn wait(
    child: &mut Child,
    readers: &mut Vec<Thread<'_>>,
    by: Deadline,
) -> io::Result<Option<ExitStatus>> {
    let mut pause = FIRST_PAUSE;
    loop {
        for reader in readers.extract_if(.., |reader| reader.is_finished()) {
            joined(reader)?;
        }
        // Reaped only once its output has closed, so that its group, which a process it started
        // may still be in, is killed, if it must be, while `child`, which leads it, holds its ID.
        if readers.is_empty()
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

fn joined(thread: Thread<'_>) -> io::Result<()> {
    thread
        .join()
        .unwrap_or_else(|panic| panic::resume_unwind(panic))
}
