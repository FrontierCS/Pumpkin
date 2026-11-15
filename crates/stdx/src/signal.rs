//! Signals that this process notes rather than acts on: [`catch`] has each noted, and [`caught`]
//! reads the first.

use std::sync::atomic::{AtomicI32, Ordering};
use std::{io, mem, ptr};

use rustix::process::Signal;

/// The number of the first signal that [`note`] was called for, 0 until one is.
// Shared by the handler, on whatever thread a signal lands, and the threads that ask [`caught`].
static CAUGHT: AtomicI32 = AtomicI32::new(0);

/// Has each of `signals` noted, for [`caught`] to read, instead of what it would do.
///
/// Each that this process started with ignored stays ignored. A call that a noted signal lands in
/// goes on once it is noted, rather than failing with `EINTR`. Fails for a signal whose action
/// can't be set, as SIGKILL's can't be.
#[expect(
    unsafe_code,
    reason = "neither `std` nor `rustix` can set a signal's action, which `sigaction` does"
)]
pub fn catch(signals: &[Signal]) -> io::Result<()> {
    for signal in signals {
        let number = signal.as_raw();
        // SAFETY: a zeroed `sigaction` is a valid value of the C struct; given no new action,
        // `sigaction` only writes the current one into `current`, which outlives the call.
        let (read, current) = unsafe {
            let mut current: libc::sigaction = mem::zeroed();
            let read = libc::sigaction(number, ptr::null(), &raw mut current);
            (read, current)
        };
        check(read)?;
        // Whatever started this process with the signal ignored meant it to outlive the signal,
        // as `nohup` means for SIGHUP.
        if current.sa_sigaction == libc::SIG_IGN {
            continue;
        }
        // SAFETY: a zeroed `sigaction` is a valid value of the C struct, whose mask `sigemptyset`
        // empties; `sigaction` only reads it; and `note` is an `extern "C" fn(c_int)` that only
        // exchanges an atomic, which is async-signal-safe.
        let set = unsafe {
            let mut action: libc::sigaction = mem::zeroed();
            let handler: extern "C" fn(libc::c_int) = note;
            action.sa_sigaction = handler as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&raw mut action.sa_mask);
            libc::sigaction(number, &raw const action, ptr::null_mut())
        };
        check(set)?;
    }
    Ok(())
}

/// The first signal noted since [`catch`] was called for it, if one was.
pub fn caught() -> Option<Signal> {
    let number = CAUGHT.load(Ordering::SeqCst);
    if number == 0 {
        return None;
    }
    let signal = Signal::from_named_raw(number);
    Some(signal.unwrap_or_else(|| {
        unreachable!(
            "signal {number} was noted, though only `note` stores a number, and only for a \
             `Signal`, which names a signal"
        )
    }))
}

/// The handler [`catch`] sets: notes signal `number`, unless one was noted before.
extern "C" fn note(number: libc::c_int) {
    // Failing to exchange means that a signal was noted before, which stays the one noted.
    let _ = CAUGHT.compare_exchange(0, number, Ordering::SeqCst, Ordering::SeqCst);
}

fn check(result: libc::c_int) -> io::Result<()> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
