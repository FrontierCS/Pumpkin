//! Children that run on their own, which their caller follows to their end: their start, and a
//! kill of the process group one leads.

// A child that is to outlive a bounded run starts apart from one, as TigerBeetle's `spawn` does,
// beside its `exec`:
// https://github.com/tigerbeetle/tigerbeetle/blob/3cd7cf4799f65c81ffbc36eda788b8b26fd7da72/src/shell.zig#L587-L612

use std::io;
use std::process::{Child, Command, ExitStatus, Stdio};

use rustix::io::Errno;
use rustix::process::{Pid, Signal};

use super::Failure;
use super::command::name;

/// Starts `command` with `stdin`, `stdout` and `stderr` as its standard input, output and error.
///
/// It runs until its caller has it end: no bound ends it. An error says why it didn't start, as a
/// [`Failure::Unstarted`] does.
pub fn spawn(
    command: &mut Command,
    stdin: Stdio,
    stdout: Stdio,
    stderr: Stdio,
) -> Result<Child, String> {
    #[expect(
        clippy::disallowed_methods,
        reason = "the start that `clippy.toml` names, which makes its caller state each stream"
    )]
    let spawned = command.stdin(stdin).stdout(stdout).stderr(stderr).spawn();
    spawned.map_err(|error| Failure::unstarted(command, name(command), error).to_string())
}

/// Kills every process in the group that `child`, unreaped, leads, and reaps `child`, giving how
/// it exited.
pub(super) fn kill(child: &mut Child) -> io::Result<ExitStatus> {
    // `Child::id` casts the child's pid_t to the u32 it returns, which `cast_signed` undoes.
    kill_group(child.id().cast_signed())?;
    child.wait()
}

/// Kills every process left in the process group `group`, by SIGKILL; finding none left is no
/// failure.
///
/// # Panics
///
/// Panics if `group` is 1 or less, which names no single process group.
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
