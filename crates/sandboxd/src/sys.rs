//! The system calls the standard library doesn't offer, and the flags it doesn't name: waiting
//! for any child or finding whether there is one, killing a process group or every process,
//! making the process non-dumpable, opening only a regular file, at a path the role's code can
//! write, and finding whether a connection's other end has closed it, without reading from it.
//!
//! Every errno a call returns is the world's, and returned: the kernel's security policy, a
//! security module's or a seccomp filter's, may fail any call with any errno.

#![expect(
    unsafe_code,
    reason = "libc has no safe wrappers for the wait, kill, prctl and recv calls"
)]

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;

use stdx::Context;

/// Waits until a child has exited, or a child that made this process its tracer has stopped,
/// without reaping it, and returns its process ID: `None` if there are no children. Fails with
/// waitid's errno.
pub(crate) fn wait_for_child() -> io::Result<Option<i32>> {
    loop {
        // SAFETY: siginfo_t is plain data, for which all zeros is a valid value.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let options = libc::WEXITED | libc::WNOWAIT;
        // SAFETY: waitid writes only to `info`, which outlives the call.
        if unsafe { libc::waitid(libc::P_ALL, 0, &raw mut info, options) } == 0 {
            // SAFETY: waitid filled `info` for a child that exited or stopped, so it holds a
            // process ID.
            return Ok(Some(unsafe { info.si_pid() }));
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ECHILD) => return Ok(None),
            Some(libc::EINTR) => {}
            _ => return Err(error),
        }
    }
}

/// Whether this process has any children: running, stopped, or exited and not yet reaped.
/// Fails with waitid's errno.
pub(crate) fn has_children() -> io::Result<bool> {
    // SAFETY: siginfo_t is plain data, for which all zeros is a valid value.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let options = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    // SAFETY: waitid writes only to `info`, which outlives the call.
    if unsafe { libc::waitid(libc::P_ALL, 0, &raw mut info, options) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ECHILD) => Ok(false),
        _ => Err(error),
    }
}

/// Reaps child `pid` if it has exited and returns its raw status: `None` if it hasn't exited
/// yet, or was already reaped. For a child that made this process its tracer, it also returns
/// the status of a stop, and consumes the report of it, so that no wait returns it again.
/// Fails with waitpid's errno.
pub(crate) fn reap(pid: i32) -> io::Result<Option<i32>> {
    let mut status = 0;
    // SAFETY: waitpid writes only to `status`, which outlives the call.
    match unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) } {
        reaped if reaped == pid => Ok(Some(status)),
        0 => Ok(None),
        -1 => {
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::ECHILD) => Ok(None),
                _ => Err(error),
            }
        }
        #[expect(
            clippy::unreachable,
            reason = "given a child's process ID, waitpid returns that ID, 0 or -1"
        )]
        returned => unreachable!("waitpid({pid}, WNOHANG) returned {returned}"),
    }
}

/// Kills the process group `group`, if any of it is left. Fails if this process may kill none
/// of the group's processes.
pub(crate) fn kill_group(group: i32) -> io::Result<()> {
    // SAFETY: killpg takes only integers and touches no memory of this process.
    killed(unsafe { libc::killpg(group, libc::SIGKILL) })
}

/// Kills every process this one may signal, and skips the others. As the first process of its
/// namespace, that is every other process in it: the kernel never sends a signal for `-1` to
/// the sender itself or to PID 1. Where something other than the user's rights keeps it from
/// killing a process, such as a security module's policy, it may fail, or skip that one too.
pub(crate) fn kill_all_others() -> io::Result<()> {
    // SAFETY: kill takes only integers and touches no memory of this process.
    killed(unsafe { libc::kill(-1, libc::SIGKILL) })
}

/// Whether this is the first process of its PID namespace, as `sandboxd` is in a sandbox.
pub(crate) fn is_first_process() -> bool {
    std::process::id() == 1
}

/// Makes this process non-dumpable, so that the commands it starts, though they run as the
/// same user, can't reach it through `/proc`: the kernel then gives root the process's
/// `/proc/<pid>` entries, and lets only a process with `CAP_SYS_PTRACE` trace it, read its
/// memory, or open its files through `/proc/<pid>/fd`. That matters most for a container's
/// first process, whose standard output and error are the container's log, kept on the host:
/// a command that could open them could write there, outside the session.
///
/// The commands it starts don't inherit the flag, since `execve` resets it. Fails if the
/// kernel refuses, as a security module's policy may.
pub(crate) fn make_non_dumpable() -> io::Result<()> {
    let off: libc::c_ulong = 0;
    // SAFETY: `PR_SET_DUMPABLE` takes integer arguments only, each passed as the
    // `unsigned long` the kernel reads.
    match unsafe { libc::prctl(libc::PR_SET_DUMPABLE, off, off, off, off) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Opens the regular file at `path` with `options`, and fails for anything else there.
/// `sandboxd` runs as the same user as the role's code, which can put anything at the paths
/// `sandboxd` opens, so this adds to `options`:
///
/// - `O_NOFOLLOW`, so that a symbolic link there fails the open instead of leading elsewhere,
///   such as to `/proc/self/fd/2`, `sandboxd`'s own standard error, which
///   [`make_non_dumpable`] otherwise keeps from the role's code;
/// - `O_NONBLOCK`, so that a FIFO fails the open, or is opened at once and refused, instead of
///   blocking it until the FIFO's other end is opened;
/// - `O_NOCTTY`, so that a terminal, which a link in a parent directory can lead to, can't
///   become `sandboxd`'s controlling one.
///
/// The file stays non-blocking, which Linux ignores for a regular file's reads and writes, so
/// a command may be handed it as its standard input, output or error.
pub(crate) fn open_regular(path: &Path, options: &mut OpenOptions) -> Result<File, String> {
    let file = options
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .context(path.display())?;
    if file.metadata().context(path.display())?.is_file() {
        Ok(file)
    } else {
        Err(format!(
            "{} is not a regular file: name one",
            path.display()
        ))
    }
}

/// Whether the other end of `connection` has closed it, as a killed process's sockets close:
/// whether reading it would find its end, or fail. It reads nothing, and doesn't wait.
pub(crate) fn peer_closed(connection: &UnixStream) -> bool {
    let mut byte = 0_u8;
    let flags = libc::MSG_PEEK | libc::MSG_DONTWAIT;
    // SAFETY: recv writes at most one byte, to `byte`, which outlives the call.
    match unsafe { libc::recv(connection.as_raw_fd(), (&raw mut byte).cast(), 1, flags) } {
        0 => true,
        1.. => false,
        _ => io::Error::last_os_error().kind() != io::ErrorKind::WouldBlock,
    }
}

/// The outcome of a `kill` or `killpg` that sent `SIGKILL` and returned `result`: finding no
/// process left to kill is no failure, and any other errno is the world's, as when the user may
/// not kill a process (`EPERM`, which `kill(-1)` never reports), or a security module's policy
/// forbids it.
fn killed(result: libc::c_int) -> io::Result<()> {
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(()),
        _ => Err(error),
    }
}
