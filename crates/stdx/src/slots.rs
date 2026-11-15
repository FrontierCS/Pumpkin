//! Slots that processes take, at most their count at once ([`take`]).
//!
//! A slot is held as a counting semaphore's is, but by a lock on a file, which the kernel lets
//! go however its holder ends.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write as _;
use std::num::NonZeroUsize;
use std::path::Path;
use std::thread;
use std::time::Duration;

use crate::Context as _;

// How long a taker waits, once each slot is held, before it looks again. What a slot is held for
// lasts seconds or minutes.
const POLL: Duration = Duration::from_millis(500);

/// One of `count` slots, the files `0` to `count - 1` of `dir`, locked by the file returned.
///
/// It makes `dir` if need be, and takes the first slot that none holds, or, while each is held,
/// the first let go, looked for every `POLL`, and names `holder` in it. If each is held at
/// first, `waiting` hears what each holder named, once, before the wait. Fails with `waiting`'s
/// error, if it fails.
pub fn take(
    dir: &Path,
    count: NonZeroUsize,
    holder: &str,
    waiting: impl FnOnce(&[String]) -> Result<(), String>,
) -> Result<File, String> {
    fs::create_dir_all(dir).context(dir.display())?;
    let mut waiting = Some(waiting);
    loop {
        let mut holders = Vec::new();
        for index in 0..count.get() {
            let path = dir.join(index.to_string());
            let opened = (OpenOptions::new().read(true).write(true).create(true))
                .truncate(false)
                .open(&path);
            let mut slot = opened.context(path.display())?;
            match slot.try_lock() {
                Ok(()) => {
                    let named = slot.set_len(0);
                    let named = named.and_then(|()| slot.write_all(holder.as_bytes()));
                    named.context(path.display())?;
                    return Ok(slot);
                }
                // A holder that has yet to write its name is named by its slot's.
                Err(TryLockError::WouldBlock) => {
                    let named = fs::read_to_string(&path).context(path.display())?;
                    holders.push(if named.is_empty() {
                        index.to_string()
                    } else {
                        named
                    });
                }
                Err(TryLockError::Error(error)) => return Err(error).context(path.display()),
            }
        }
        if let Some(waiting) = waiting.take() {
            waiting(&holders)?;
        }
        thread::sleep(POLL);
    }
}
