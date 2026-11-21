//! Children that run on their own: a wait for the first of them to exit, and a kill of the
//! process group one leads.

use std::process::{Child, ExitStatus};
use std::{io, thread};

use rustix::io::Errno;
use rustix::process::{Pid, Signal};

use super::{FIRST_PAUSE, MAX_PAUSE};

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
pub(super) fn kill(child: &mut Child) -> io::Result<()> {
    // `Child::id` casts the child's pid_t to the u32 it returns, which `cast_signed` undoes.
    kill_group(child.id().cast_signed())?;
    child.wait().map(drop)
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
