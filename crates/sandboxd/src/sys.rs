//! The system calls the standard library doesn't offer, and the flags it doesn't name: waiting
//! for any child or finding whether there is one, killing every process, finding whether the
//! process is root and becoming the role's user, making the process non-dumpable, opening only a
//! regular file, at a path the role's code can write, and finding whether a connection's other
//! end has closed it, without reading from it.
//!
//! Every errno a call returns is the world's, and returned: the kernel's security policy, a
//! security module's or a seccomp filter's, may fail any call with any errno.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;

use rustix::io::Errno;
use rustix::net::RecvFlags;
use rustix::process::{
    self, DumpableBehavior, Gid, Pid, Signal, Uid, WaitId, WaitIdOptions, WaitOptions,
};
use rustix::thread;
use stdx::Context;

/// Waits until a child has exited, or a child that made this process its tracer has stopped,
/// without reaping it: false if there are no children. Fails with waitid's errno.
pub(crate) fn wait_for_child() -> io::Result<bool> {
    loop {
        match process::waitid(WaitId::All, WaitIdOptions::EXITED | WaitIdOptions::NOWAIT) {
            Ok(_) => return Ok(true),
            Err(Errno::CHILD) => return Ok(false),
            Err(Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

/// Whether this process has any children: running, stopped, or exited and not yet reaped.
/// Fails with waitid's errno.
pub(crate) fn has_children() -> io::Result<bool> {
    let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
    match process::waitid(WaitId::All, options) {
        Ok(_) => Ok(true),
        Err(Errno::CHILD) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Reaps a child that has exited and returns its process ID and raw status: `None` if none has
/// exited. For a child that made this process its tracer, it also returns the status of a
/// stop, and consumes the report of it, so that no wait returns it again. Fails with waitpid's
/// errno.
pub(crate) fn reap() -> io::Result<Option<(i32, i32)>> {
    match process::wait(WaitOptions::NOHANG) {
        Ok(reaped) => Ok(reaped.map(|(pid, status)| (pid.as_raw_pid(), status.as_raw()))),
        Err(Errno::CHILD) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Kills every other process in this one's PID namespace that it may signal, as the first
/// process of its namespace, and skips the others: each that `/proc` lists. A process that
/// another forks while `/proc` is read may be missed; freezing kills again each time a child is
/// reaped ([`Commands::freeze`](crate::commands::Commands::freeze)). Where something other than
/// the user's rights keeps it from killing a process, such as a security module's policy, it may
/// fail. Every process is tried, and the first failure is returned.
pub(crate) fn kill_all_others() -> io::Result<()> {
    let own = process::getpid();
    let mut outcome = Ok(());
    for entry in fs::read_dir("/proc")? {
        // Each process is listed by its ID, among files of other names.
        let name = entry?.file_name();
        let pid = (name.to_str()).and_then(|name| name.parse().ok());
        let Some(pid) = pid.and_then(Pid::from_raw).filter(|&pid| pid != own) else {
            continue;
        };
        // One this process may not signal is skipped, as is one gone since `/proc` listed it.
        let killed = match process::kill_process(pid, Signal::KILL) {
            Ok(()) | Err(Errno::PERM | Errno::SRCH) => Ok(()),
            Err(error) => Err(error.into()),
        };
        outcome = outcome.and(killed);
    }
    outcome
}

/// Whether this is the first process of its PID namespace, as `sandboxd` is in a sandbox.
pub(crate) fn is_first_process() -> bool {
    std::process::id() == 1
}

/// Whether this process's effective user is root.
pub(crate) fn is_root() -> bool {
    process::geteuid().is_root()
}

/// Makes this process, started as root, `uid` and `gid` alone, for good: it sets
/// `no_new_privs`, so that no program it or its commands execute gains a privilege, a set-user-ID
/// one's included, then drops every supplementary group, then makes `gid` its real, effective
/// and saved group, then `uid` its user likewise, which takes every capability it had. Call it
/// before any thread starts: each call changes the calling thread alone. Fails with the errno of
/// the first call that fails.
pub(crate) fn become_user(uid: u32, gid: u32) -> io::Result<()> {
    let (uid, gid) = (Uid::from_raw(uid), Gid::from_raw(gid));
    thread::set_no_new_privs(true)?;
    thread::set_thread_groups(&[])?;
    thread::set_thread_res_gid(gid, gid, gid)?;
    thread::set_thread_res_uid(uid, uid, uid)?;
    Ok(())
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
    Ok(process::set_dumpable_behavior(
        DumpableBehavior::NotDumpable,
    )?)
}

/// Opens the regular file at `path` with `options`, and fails for anything else there, a
/// symbolic link or a FIFO included. The file stays non-blocking, which Linux ignores for a
/// regular file, so a command may be handed it as its standard input, output or error.
pub(crate) fn open_regular(path: &Path, options: &mut OpenOptions) -> Result<File, String> {
    // The role's code, which runs as this user, can put anything at the paths `sandboxd` opens:
    // a link, such as to `/proc/self/fd/2`, which `make_non_dumpable` keeps from it; a FIFO,
    // which would block the open; or a terminal, which would become the controlling one.
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
    let mut byte = [0_u8];
    match rustix::net::recv(connection, &mut byte, RecvFlags::PEEK | RecvFlags::DONTWAIT) {
        Ok((read, _)) => read == 0,
        Err(error) => error != Errno::WOULDBLOCK,
    }
}
