//! The system calls the standard library doesn't offer, and the flags it doesn't name: waiting
//! for any child or finding whether there is one, killing a process group or every process,
//! making the process non-dumpable, and opening only a regular file, at a path the role's code
//! can write.

#![expect(
    unsafe_code,
    reason = "libc has no safe wrappers for the wait, kill and prctl calls"
)]

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use stdx::Context;

/// Waits until a child has exited, or a child that made this process its tracer has stopped,
/// without reaping it, and returns its process ID: `None` if there are no children.
pub(crate) fn wait_for_child() -> Option<i32> {
    loop {
        // SAFETY: siginfo_t is plain data, for which all zeros is a valid value.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let options = libc::WEXITED | libc::WNOWAIT;
        // SAFETY: waitid writes only to `info`, which outlives the call.
        if unsafe { libc::waitid(libc::P_ALL, 0, &raw mut info, options) } == 0 {
            // SAFETY: waitid filled `info` for a child that exited or stopped, so it holds a
            // process ID.
            return Some(unsafe { info.si_pid() });
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ECHILD) => return None,
            Some(libc::EINTR) => {}
            #[expect(
                clippy::unreachable,
                reason = "waitid fails otherwise only for invalid options or a nonblocking \
                          pidfd, and these options are valid and name no pidfd"
            )]
            _ => unreachable!("waitid(P_ALL, WEXITED | WNOWAIT) failed: {error}"),
        }
    }
}

/// Whether this process has any children: running, stopped, or exited and not yet reaped.
pub(crate) fn has_children() -> bool {
    // SAFETY: siginfo_t is plain data, for which all zeros is a valid value.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let options = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    // SAFETY: waitid writes only to `info`, which outlives the call.
    if unsafe { libc::waitid(libc::P_ALL, 0, &raw mut info, options) } == 0 {
        return true;
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ECHILD) => false,
        #[expect(
            clippy::unreachable,
            reason = "waitid fails otherwise only for invalid options, a nonblocking pidfd, or a \
                      signal while it blocks, and these options are valid, name no pidfd, and \
                      don't block"
        )]
        _ => unreachable!("waitid(P_ALL, WEXITED | WNOHANG | WNOWAIT) failed: {error}"),
    }
}

/// Reaps child `pid` if it has exited and returns its raw status: `None` if it hasn't exited
/// yet, or was already reaped. For a child that made this process its tracer, it also returns
/// the status of a stop, and consumes the report of it, so that no wait returns it again.
pub(crate) fn reap(pid: i32) -> Option<i32> {
    let mut status = 0;
    // SAFETY: waitpid writes only to `status`, which outlives the call.
    match unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) } {
        reaped if reaped == pid => Some(status),
        0 => None,
        returned => {
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::ECHILD) => None,
                #[expect(
                    clippy::unreachable,
                    reason = "given a child's process ID and WNOHANG, waitpid returns that ID or \
                              0, or fails with ECHILD: EINTR needs a wait that blocks, EINVAL \
                              invalid options, and ESRCH a pid of INT_MIN"
                )]
                _ => unreachable!("waitpid({pid}, WNOHANG) returned {returned}: {error}"),
            }
        }
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
/// the sender itself or to PID 1.
pub(crate) fn kill_all_others() {
    // SAFETY: kill takes only integers and touches no memory of this process.
    match killed(unsafe { libc::kill(-1, libc::SIGKILL) }) {
        Ok(()) => {}
        #[expect(
            clippy::unreachable,
            reason = "Linux's kill(-1) skips each process it may not signal, and returns success, \
                      or ESRCH when it found nothing to signal: never EPERM"
        )]
        Err(error) => unreachable!("kill(-1, SIGKILL) failed: {error}"),
    }
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
/// The commands it starts don't inherit the flag, since `execve` resets it.
pub(crate) fn make_non_dumpable() {
    let off: libc::c_ulong = 0;
    // SAFETY: `PR_SET_DUMPABLE` takes integer arguments only, each passed as the
    // `unsigned long` the kernel reads.
    match unsafe { libc::prctl(libc::PR_SET_DUMPABLE, off, off, off, off) } {
        0 => {}
        #[expect(
            clippy::unreachable,
            reason = "PR_SET_DUMPABLE fails only for a value other than SUID_DUMP_DISABLE (0) or \
                      SUID_DUMP_USER (1), and this passes 0"
        )]
        _ => unreachable!(
            "prctl(PR_SET_DUMPABLE, 0) failed: {}",
            io::Error::last_os_error()
        ),
    }
}

/// Opens the regular file at `path` with `options`, and fails for anything else there.
/// `sandboxd` runs as the same user as the role's code, which can plant anything at the paths
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
/// a command may be handed it as its standard output or error.
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

/// The outcome of a `kill` or `killpg` that sent `SIGKILL` and returned `result`: finding no
/// process left to kill is no failure, and lacking the permission to kill is the only one,
/// which only [`kill_group`] can report.
fn killed(result: libc::c_int) -> io::Result<()> {
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(()),
        Some(libc::EPERM) => Err(error),
        #[expect(
            clippy::unreachable,
            reason = "kill and killpg fail otherwise only with EINVAL, for an invalid signal, and \
                      both send SIGKILL"
        )]
        _ => unreachable!("sending SIGKILL failed: {error}"),
    }
}
