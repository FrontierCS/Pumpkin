use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

use stdx::time::Clock;

use crate::{Session, lock};

pub(crate) struct Ended {
    recorded: OnceLock<(String, Duration)>,
    // The session's clock, which whichever thread records the end reads.
    now: Box<dyn Fn() -> Duration + Send + Sync>,
}

impl Ended {
    pub(crate) fn new(clock: impl Clock + Send + Sync + 'static) -> Self {
        Self {
            recorded: OnceLock::new(),
            now: Box::new(move || clock.now()),
        }
    }

    /// Records the end for `reason` now, unless an end is recorded already, and returns the reason
    /// recorded.
    pub(crate) fn record(&self, reason: String) -> &str {
        let (recorded, _) = self.recorded.get_or_init(|| (reason, (self.now)()));
        recorded
    }

    pub(crate) fn get(&self) -> Option<(&str, Duration)> {
        let (reason, at) = self.recorded.get()?;
        Some((reason, *at))
    }
}

impl fmt::Debug for Ended {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("Ended").field(&self.get()).finish()
    }
}

impl<C> Session<C> {
    /// Why the session has closed, and when by its clock, as the first of its threads to see the
    /// end saw it. That includes a failure already seen by a request whose
    /// [`Arrival::Ended`](crate::Arrival::Ended) is still on its way to the owner. Once the gateway
    /// has closed the connection, this says why.
    pub fn ended(&self) -> Option<(&str, Duration)> {
        // Under the routes' lock, which `end` holds from closing the connection to recording why.
        let _routes = lock(&self.shared.routes);
        self.shared.ended.get()
    }
}
