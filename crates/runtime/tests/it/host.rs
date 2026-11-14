//! A simulated host ([`SimHost`]), whose clock moves only as its script says, so that a test
//! decides when the run's loop takes an input, how long a sync of its record lasts, whether it
//! fails, and what time the run reads then, with no real waiting; and which tells a
//! [`Journal`] each sync, in order with what the simulated provider tells it.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use runtime::{Clock, Durable, Host, Inbox};

use crate::sim::Journal;

/// The longest a simulated host waits in real time for an input: one that its clock alone would
/// bring never comes, so a wait this long is the test's bug.
const WATCHDOG: Duration = Duration::from_secs(60);

/// What a wait past [`WATCHDOG`] expected of the test, but for a slow sync's.
const SCRIPTED: &str = "the test to make every input the run waits on come, as its clock moves \
                        only by script";

/// A host whose clock reads 0 from the run's start until its script moves it: its late take
/// ([`LateTake`]), or its slow sync ([`SlowSync`]). A wait takes an input as soon as one comes
/// while its time is to come, and what the inbox holds, without waiting, once its time has
/// passed, as the clock never moves by itself, but for the takes that the late take says wait. A
/// sync makes nothing last, and succeeds unless the script fails it ([`SimHost::failed_sync`]).
///
/// # Panics
///
/// A wait panics once it has waited [`WATCHDOG`] for an input.
pub(crate) struct SimHost {
    /// Shared, as the clock's clones read it on the run's threads, a judge call's, a model
    /// request's or a session's reader too, and its inboxes take the late take from it on theirs,
    /// which then moves the clock.
    state: Arc<Mutex<State>>,
    /// Where each sync goes, if anywhere.
    journal: Option<Journal>,
}

struct State {
    /// The time since the run started.
    now: Duration,
    /// The late take, until it comes.
    late: Option<LateTake>,
    /// The slow sync, until it comes.
    slow: Option<SlowSync>,
    /// The record whose sync fails, until it does.
    failed: Option<u64>,
    /// The name of the file of which a sync fails, and how many of its syncs come first, until
    /// it does.
    failed_file: Option<(&'static str, u32)>,
    /// The inbox the clock made first, the loop's, as a run makes it as it starts its clock.
    first: Option<Arc<dyn Fill>>,
}

/// The run's first take of an input, from whichever of its inboxes, comes late: once `behind`
/// more inputs are queued behind it, and at `at`, as if the loop had been busy until then. Then
/// each of the next `then` takes from that inbox waits for an input, whatever the time, as if the
/// loop had been busy until one came.
pub(crate) struct LateTake {
    pub(crate) behind: usize,
    pub(crate) at: Duration,
    pub(crate) then: usize,
}

/// The first sync of the run log that makes `record` last, by its `seq`, with the records before
/// it, lasts until `queued` inputs are queued in the loop's inbox, and ends at `at`, as the
/// machine's disk may hold a sync while other threads hand the loop what they observed.
pub(crate) struct SlowSync {
    pub(crate) record: u64,
    pub(crate) queued: usize,
    pub(crate) at: Duration,
}

impl SimHost {
    pub(crate) fn new() -> Self {
        let state = State {
            now: Duration::ZERO,
            late: None,
            slow: None,
            failed: None,
            failed_file: None,
            first: None,
        };
        Self {
            state: Arc::new(Mutex::new(state)),
            journal: None,
        }
    }

    pub(crate) fn late_take(self, late: LateTake) -> Self {
        stdx::unpoisoned(self.state.lock()).late = Some(late);
        self
    }

    pub(crate) fn slow_sync(self, slow: SlowSync) -> Self {
        stdx::unpoisoned(self.state.lock()).slow = Some(slow);
        self
    }

    /// The host, whose first sync of the run log that makes `record` last fails.
    pub(crate) fn failed_sync(self, record: u64) -> Self {
        stdx::unpoisoned(self.state.lock()).failed = Some(record);
        self
    }

    /// The host, whose sync of a file named `name`, after `before` syncs of files of that name,
    /// fails.
    pub(crate) fn failed_sync_of(self, name: &'static str, before: u32) -> Self {
        stdx::unpoisoned(self.state.lock()).failed_file = Some((name, before));
        self
    }

    /// The host, which tells `journal` each sync as it ends: `fdatasync <path>` or `fsync
    /// <path>`, as [`Durable`] says, followed for the run log by the `seq` of its last record,
    /// and by how the script held or failed it.
    pub(crate) fn journaled(mut self, journal: &Journal) -> Self {
        self.journal = Some(journal.clone());
        self
    }
}

impl Host for SimHost {
    type Clock = SimClock;

    fn start(&self) -> SimClock {
        SimClock {
            state: Arc::clone(&self.state),
            unix: SystemTime::now(),
        }
    }

    fn sync(&self, file: &File, durable: Durable) -> io::Result<()> {
        // A file knows no path of its own, but the kernel's table of the process's files does.
        let path = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
        let kind = match durable {
            Durable::Data => "fdatasync",
            Durable::All => "fsync",
        };
        let last = last_record(&path)?;
        let through = last.map_or_else(String::new, |last| format!(", through record {last}"));
        let lasts = |record: u64| last.is_some_and(|last| last >= record);
        let mut state = stdx::unpoisoned(self.state.lock());
        let slow = state.slow.take_if(|slow| lasts(slow.record));
        let named = |name: &str| path.file_name() == Some(name.as_ref());
        let file = match &mut state.failed_file {
            Some((name, 0)) => named(name),
            Some((name, before)) => {
                if named(name) {
                    *before -= 1;
                }
                false
            }
            None => false,
        };
        if file {
            state.failed_file = None;
        }
        let failed = state.failed.take_if(|record| lasts(*record)).is_some() || file;
        let first = state.first.clone();
        // Without the lock, so that the run's other threads read the clock as they hand the loop
        // its inputs.
        drop(state);
        let held = slow.map_or_else(String::new, |SlowSync { queued, at, .. }| {
            #[expect(clippy::expect_used, reason = "a test's bug fails the test")]
            first
                .expect("a slow sync of the run log comes once the run's clock has made an inbox")
                .fill(
                    queued,
                    "the test to make every input the slow sync waits on come into the loop's \
                     inbox, which the host takes to be the inbox the clock made first",
                );
            stdx::unpoisoned(self.state.lock()).now = at;
            format!(", held until the loop's inbox held {queued}, to {at:?}")
        });
        let outcome = if failed { ", failed" } else { "" };
        if let Some(journal) = &self.journal {
            journal.tell(format!("{kind} {}{through}{held}{outcome}", path.display()));
        }
        if failed {
            return Err(io::Error::other("the simulated host's sync failed"));
        }
        Ok(())
    }
}

/// The `seq` of the last record of the run log at `path`, if it is the log and has one.
fn last_record(path: &Path) -> io::Result<Option<u64>> {
    if path.file_name() != Some("log.jsonl".as_ref()) {
        return Ok(None);
    }
    let records = fs::read_to_string(path)?.lines().count();
    Ok(u64::try_from(records)
        .ok()
        .and_then(|records| records.checked_sub(1)))
}

/// [`SimHost`]'s clock of a run.
#[derive(Clone)]
pub(crate) struct SimClock {
    state: Arc<Mutex<State>>,
    /// When the run started, by the machine's clock, which its record holds.
    unix: SystemTime,
}

impl Clock for SimClock {
    type Inbox<T: Send + 'static> = SimInbox<T>;

    fn now(&self) -> Duration {
        stdx::unpoisoned(self.state.lock()).now
    }

    fn unix(&self) -> SystemTime {
        self.unix
    }

    fn inbox<T: Send + 'static>(&self) -> SimInbox<T> {
        let (sender, receiver) = mpsc::channel();
        let queue = Arc::new(Mutex::new(Queue {
            receiver,
            behind: VecDeque::new(),
        }));
        let mut state = stdx::unpoisoned(self.state.lock());
        if state.first.is_none() {
            state.first = Some(Arc::clone(&queue) as Arc<dyn Fill>);
        }
        SimInbox {
            clock: self.clone(),
            queue,
            sender,
            then: 0,
        }
    }
}

/// [`SimClock`]'s inbox.
pub(crate) struct SimInbox<T> {
    clock: SimClock,
    /// Shared with the host, which fills the loop's as a slow sync holds the loop.
    queue: Arc<Mutex<Queue<T>>>,
    // Kept, so that the inbox never disconnects, whoever else holds a sender.
    sender: Sender<T>,
    /// How many more takes wait for an input, whatever the time, as the late take says.
    then: usize,
}

/// What an inbox holds: what was received of its channel ahead of its takes, then the channel.
struct Queue<T> {
    receiver: Receiver<T>,
    /// What a late take or a slow sync received, which the takes after it return first.
    behind: VecDeque<T>,
}

/// An inbox's queue, whatever it holds.
trait Fill: Send + Sync {
    /// Waits until the queue holds `queued` inputs received ahead of its takes, panicking past
    /// [`WATCHDOG`] with what the wait `expected` of the test.
    fn fill(&self, queued: usize, expected: &str);
}

impl<T: Send> Fill for Mutex<Queue<T>> {
    fn fill(&self, queued: usize, expected: &str) {
        let mut queue = stdx::unpoisoned(self.lock());
        while queue.behind.len() < queued {
            let next = queue.received(expected);
            queue.behind.push_back(next);
        }
    }
}

impl<T: Send> Inbox<T> for SimInbox<T> {
    fn sender(&self) -> Sender<T> {
        self.sender.clone()
    }

    fn wait(&mut self, until: Duration) -> Option<T> {
        self.next(self.clock.now() < until)
    }

    fn take(&mut self) -> Option<T> {
        self.next(false)
    }
}

impl<T: Send> SimInbox<T> {
    /// The next input taken: what the late take or a slow sync received ahead of it first, then
    /// what the channel holds, once it comes if `waits` or the late take says this take waits.
    fn next(&mut self, waits: bool) -> Option<T> {
        let waits = waits || self.then > 0;
        self.then = self.then.saturating_sub(1);
        let mut queue = stdx::unpoisoned(self.queue.lock());
        let input = match queue.behind.pop_front() {
            Some(input) => input,
            None if waits => queue.received(SCRIPTED),
            None => match queue.receiver.try_recv() {
                Ok(input) => input,
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => disconnected(),
            },
        };
        drop(queue);
        Some(self.late(input))
    }

    /// `input`, taken: if this is the late take, once the inputs behind it have come, and with
    /// the clock moved to its time.
    fn late(&mut self, input: T) -> T {
        let late = stdx::unpoisoned(self.clock.state.lock()).late.take();
        let Some(LateTake { behind, at, then }) = late else {
            return input;
        };
        self.then = then;
        // Without the clock's lock, so that the run's other threads read the clock as they wait.
        self.queue.fill(behind, SCRIPTED);
        stdx::unpoisoned(self.clock.state.lock()).now = at;
        input
    }
}

impl<T> Queue<T> {
    /// The next input the channel holds, once it comes, panicking past [`WATCHDOG`] with what
    /// the wait `expected` of the test.
    fn received(&self, expected: &str) -> T {
        match self.receiver.recv_timeout(WATCHDOG) {
            Ok(input) => input,
            #[expect(clippy::panic, reason = "a test's bug fails the test")]
            Err(RecvTimeoutError::Timeout) => panic!(
                "the simulated host waited {WATCHDOG:?} for an input that never came: expected \
                 {expected}"
            ),
            Err(RecvTimeoutError::Disconnected) => disconnected(),
        }
    }
}

fn disconnected() -> ! {
    unreachable!(
        "an inbox disconnected, though it holds a sender of its own, so its channel can't \
         disconnect"
    )
}
