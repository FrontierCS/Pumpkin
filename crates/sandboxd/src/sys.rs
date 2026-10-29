//! The system calls the standard library doesn't offer: waiting for any child, killing a
//! process group or every process, and making the process non-dumpable.

#![expect(
    unsafe_code,
    reason = "libc has no safe wrappers for waitpid, kill and prctl"
)]

use std::io;

/// Waits until a child has exited, without reaping it, and returns its process ID.
///
/// # Errors
///
/// Fails with `ECHILD` when there are no children.
pub(crate) fn wait_for_exit() -> io::Result<i32> {
    loop {
        // SAFETY: siginfo_t is plain data, for which all zeros is a valid value.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let options = libc::WEXITED | libc::WNOWAIT;
        // SAFETY: waitid writes only to `info`, which outlives the call.
        if unsafe { libc::waitid(libc::P_ALL, 0, &raw mut info, options) } == 0 {
            // SAFETY: waitid filled `info` for an exited child, so it holds a process ID.
            return Ok(unsafe { info.si_pid() });
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Reaps child `pid` if it has exited and returns its raw status: `None` if it was already
/// reaped.
pub(crate) fn reap(pid: i32) -> io::Result<Option<i32>> {
    let mut status = 0;
    // SAFETY: waitpid writes only to `status`, which outlives the call.
    match unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) } {
        reaped if reaped == pid => Ok(Some(status)),
        0 => Ok(None),
        _ => {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                Ok(None)
            } else {
                Err(error)
            }
        }
    }
}

/// Kills the process group `group`, if any of it is left.
pub(crate) fn kill_group(group: i32) -> io::Result<()> {
    kill(-group, libc::SIGKILL)
}

/// Kills every process this one may signal. As the first process of its namespace, that is
/// every other process in it: the kernel never sends a signal for `-1` to the sender itself
/// or to PID 1.
pub(crate) fn kill_all_others() -> io::Result<()> {
    kill(-1, libc::SIGKILL)
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
pub(crate) fn make_non_dumpable() -> io::Result<()> {
    let off: libc::c_ulong = 0;
    // SAFETY: `PR_SET_DUMPABLE` takes integer arguments only, each passed as the
    // `unsigned long` the kernel reads.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, off, off, off, off) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn kill(pid: i32, signal: i32) -> io::Result<()> {
    // SAFETY: kill takes only integers and touches no memory of this process.
    if unsafe { libc::kill(pid, signal) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}
