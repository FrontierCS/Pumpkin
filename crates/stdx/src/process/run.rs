//! A command run to its end within the bound that its caller states, and killed at the bound,
//! or once a captured stream passes what a run holds: with what it started, where it has a process
//! group of its own.

use std::io::{self, Write};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use super::Failure;
use super::command::{name, shown};
use super::watch::{Group, watched};
use crate::Deadline;

// Every run states its bound, as each of TigerBeetle's `exec` and `exec_options` runs within a
// timeout, which `exec_options` lets its caller change:
// https://github.com/tigerbeetle/tigerbeetle/blob/3cd7cf4799f65c81ffbc36eda788b8b26fd7da72/src/shell.zig#L401-L415
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
///
/// A run with neither stream inherited leads a process group of its own, killed whole as the run
/// ends, so what the command started ends with it, but for a process that leaves the group, as
/// `setsid` does; and no signal to this process's group, such as a terminal's Ctrl-C, reaches it.
/// A run with a stream inherited stays in this process's group, so that a terminal's Ctrl-C reaches
/// it, and it reads and writes the terminal unstopped; at its bound, its command alone is killed,
/// and what the command started may outlive it.
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

/// Runs `command` to its end as `how` says, and gives its output if it exits with 0.
///
/// The command is killed at its bound, or once a captured stream passes 128 MiB, with what it
/// started where [`Stream`] says; a stream not captured is empty in the output. A command whose
/// bound leaves it no time neither starts nor echoes, and one outlives this process if it dies.
///
/// # Panics
///
/// Panics if `how`'s bound is never reached, as one past what the clock can hold is.
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
    let unstarted = |name| Failure::Unended {
        name,
        bound,
        started: false,
    };
    let by = match bound {
        Bound::Within(timeout) if timeout.is_zero() => return Err(unstarted(name)),
        Bound::Within(timeout) => Deadline::after(timeout),
        Bound::By(by) if by.remaining().is_none() => return Err(unstarted(name)),
        Bound::By(by) => by,
    };
    assert!(
        by.remaining() < Some(Duration::MAX),
        "the bound of a run of `{name}` is past what the clock can hold, so it is never reached: \
         expected one that is, as a run that is to outlive any bound starts by `spawn`"
    );
    if let Echo::Line = echo
        && let Err(error) = echoed(command)
    {
        let expected = "expected this process's standard error to take the echo".to_owned();
        return Err(Failure::Unstarted {
            name,
            error,
            expected,
        });
    }
    let stdin = input.map_or_else(Stdio::null, |_| Stdio::piped());
    command
        .stdin(stdin)
        .stdout(stdio(stdout))
        .stderr(stdio(stderr));
    let group = if matches!(stdout, Stream::Inherited) || matches!(stderr, Stream::Inherited) {
        // This process's group by its ID, as a `Command` keeps a group that an earlier run set.
        command.process_group(rustix::process::getpgrp().as_raw_nonzero().get());
        Group::Callers
    } else {
        // A group of its own, so that its kill reaches what the command started too.
        command.process_group(0);
        Group::Own
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "the start that `watched` follows to its end"
    )]
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return Err(Failure::unstarted(command, name, error)),
    };
    match watched(&mut child, input, by, group) {
        Ok(Some(output)) if output.status.success() => Ok(output),
        Ok(Some(output)) => Err(Failure::Exited {
            name,
            output,
            stderr,
        }),
        Ok(None) => Err(Failure::Unended {
            name,
            bound,
            started: true,
        }),
        Err(error) => Err(Failure::Unwatched { name, error }),
    }
}

fn echoed(command: &Command) -> io::Result<()> {
    let line = format!("$ {}\n", shown(command));
    (io::stderr().write_all(line.as_bytes()))
        .map_err(|error| io::Error::new(error.kind(), format!("echoing it failed: {error}")))
}

fn stdio(stream: Stream) -> Stdio {
    match stream {
        Stream::Captured => Stdio::piped(),
        Stream::Inherited => Stdio::inherit(),
        Stream::Discarded => Stdio::null(),
    }
}

/// What `output`'s command printed on its standard output, as text, less one trailing `\n`, then
/// one trailing `\r`, as xshell's `read` gives it; an error if it isn't UTF-8.
// https://github.com/matklad/xshell/blob/9a405946ecbbfe50fe4a7229bd764194154e4ce6/src/lib.rs#L991-L998
pub fn printed(output: Output) -> Result<String, String> {
    let mut printed = String::from_utf8(output.stdout).map_err(|error| {
        let error = error.utf8_error();
        format!("the standard output isn't UTF-8: {error}: expected text")
    })?;
    for end in ['\n', '\r'] {
        if printed.ends_with(end) {
            printed.pop();
        }
    }
    Ok(printed)
}
