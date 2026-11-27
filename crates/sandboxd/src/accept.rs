//! Taking connections at a listener for as long as the process lasts.

use std::time::Duration;
use std::{io, thread};

// A failed accept is tried again after a wait. The wait doubles from `FIRST_WAIT` up to
// `LAST_WAIT`, and starts over once an accept succeeds, as Go's `net/http` server waits:
// https://github.com/golang/go/blob/46b62495e1bf4d56e0313320772ae588d9119aed/src/net/http/server.go#L3705-L3736
// An accept fails while the process has no descriptor to spare, until a connection ends. The wait
// keeps the listener from spinning meanwhile. `LAST_WAIT` bounds how long a freed descriptor waits.
const FIRST_WAIT: Duration = Duration::from_millis(5);
const LAST_WAIT: Duration = Duration::from_secs(1);

/// Hands each connection that `accept` takes to `take`, for as long as the process lasts. After
/// `accept` fails, whatever its error, it waits before it tries again.
pub(crate) fn each<C>(mut accept: impl FnMut() -> io::Result<C>, mut take: impl FnMut(C)) -> ! {
    let mut waited = None;
    loop {
        let Ok(connection) = accept() else {
            let wait = waited.map_or(FIRST_WAIT, |last: Duration| (last * 2).min(LAST_WAIT));
            thread::sleep(wait);
            waited = Some(wait);
            continue;
        };
        waited = None;
        take(connection);
    }
}
