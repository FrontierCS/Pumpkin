//! A simulated host, [`SimHost`], whose clock moves only as its script says, so that a test
//! decides when the run's loop takes an input, how long a sync of its record lasts, whether it
//! fails, and what time the run reads then, with no real waiting. Its clock only moves forward,
//! and a move ends every wait whose time it reaches. It tells a [`Journal`] each sync, in order
//! with what the simulated provider tells it.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use runtime::{Clock, Durable, Host, Inbox};

use crate::sim::Journal;

// The longest a simulated host waits in real time for an input: one that its clock alone would
// bring never comes, so a wait this long is the test's bug.
const WATCHDOG: Duration = Duration::from_secs(60);

const SCRIPTED: &str = "the test to make every input the run waits on come, as its clock moves \
                        only by script";

// How long a wait on the clock blocks between its looks at it.
const LOOK: Duration = Duration::from_millis(10);

// When a simulated host's runs start by the machine's clock, 2026-01-01T00:00:00Z, so that a time
// of the record that the run reads from it shows exactly, as TigerBeetle's `TimeSim` derives its
// real time from its ticks:
// https://github.com/tigerbeetle/tigerbeetle/blob/68958eb757747d0fc96a79d1f9c2fa6766b67b48/src/stdx/time.zig#L260-L269
pub(crate) const EPOCH: Duration = Duration::from_hours(490_896);

/// A host whose clock reads 0 from the run's start and never moves by itself: a [`LateTake`], a
/// [`SlowSync`], a call of [`SimHost::moving_to`]'s function or the simulated provider
/// ([`SimTime`]) moves it. A wait for a time to come takes an input as soon as one comes, or ends
/// once the clock reaches its time; then it takes what the inbox holds, without waiting, but for
/// the takes that the late take says wait. A wait that neither ends fails after `WATCHDOG`. A sync
/// makes nothing last, and succeeds unless the script fails it.
pub(crate) struct SimHost {
    state: Arc<Mutex<State>>,
    journal: Option<Journal>,
}

struct State {
    now: Duration,
    late: Option<LateTake>,
    late_armed: bool,
    after_take: Option<fn()>,
    slow: VecDeque<SlowSync>,
    held: Option<(Synced, &'static str)>,
    failed: Option<Synced>,
    failed_file: Option<(&'static str, u32)>,
    first: Option<Arc<dyn Fill>>,
}

/// A take comes late, after setup or the agent's initial ops query: once `behind`
/// more inputs are queued behind it, and at `at`, as if the loop had been busy until then. During
/// setup, it waits for its input whatever the time. Then each of the next `then` takes from that
/// inbox waits for an input, whatever the time, as if the loop had been busy until one came.
pub(crate) struct LateTake {
    pub(crate) behind: usize,
    pub(crate) at: Duration,
    pub(crate) then: usize,
}

/// The first sync that makes `record` last, of the run log or another file of the record, lasts
/// until `queued` inputs are queued in the loop's inbox, and ends at `at`, as the machine's disk
/// may hold a sync while other threads hand the loop what they observed.
pub(crate) struct SlowSync {
    pub(crate) record: Synced,
    pub(crate) queued: usize,
    pub(crate) at: Duration,
}

/// What a sync makes last.
pub(crate) enum Synced {
    /// The run log's record of that `seq`, with the records before it.
    Seq(u64),
    /// A file whose path ends with the first, as it holds the second, such as a sandbox's
    /// `sandbox.json` once it records the delete, which the run log may not sync before the next
    /// act.
    Holding(&'static str, &'static str),
}

impl SimHost {
    pub(crate) fn new() -> Self {
        let state = State {
            now: Duration::ZERO,
            late: None,
            late_armed: false,
            after_take: None,
            slow: VecDeque::new(),
            held: None,
            failed: None,
            failed_file: None,
            first: None,
        };
        Self {
            state: Arc::new(Mutex::new(state)),
            journal: None,
        }
    }

    pub(crate) fn during_setup(self, late: LateTake, after: fn()) -> Self {
        let mut state = stdx::unpoisoned(self.state.lock());
        state.late_armed = true;
        state.after_take = Some(after);
        drop(state);
        self.late_take(late)
    }

    pub(crate) fn late_take(self, late: LateTake) -> Self {
        stdx::unpoisoned(self.state.lock()).late = Some(late);
        self
    }

    pub(crate) fn slow_sync(self, slow: SlowSync) -> Self {
        stdx::unpoisoned(self.state.lock()).slow.push_back(slow);
        self
    }

    /// Holds the first sync that makes `record` last until the run log holds `logged` in a line,
    /// as the machine may hold one thread while the loop records what others report.
    pub(crate) fn held_sync(self, record: Synced, logged: &'static str) -> Self {
        stdx::unpoisoned(self.state.lock()).held = Some((record, logged));
        self
    }

    /// Fails the first sync that makes `record` last.
    pub(crate) fn failed_sync(self, record: Synced) -> Self {
        stdx::unpoisoned(self.state.lock()).failed = Some(record);
        self
    }

    /// Fails the sync of a file named `name`, after `before` syncs of it that succeed.
    pub(crate) fn failed_sync_of(self, name: &'static str, before: u32) -> Self {
        stdx::unpoisoned(self.state.lock()).failed_file = Some((name, before));
        self
    }

    /// Returns a function that moves the clock to `at` each time it is called.
    pub(crate) fn moving_to(&self, at: Duration) -> impl Fn() + Send + Sync + 'static {
        let state = Arc::clone(&self.state);
        move || stdx::unpoisoned(state.lock()).move_to(at)
    }

    /// The clock, for the simulated provider and its sandboxes to read and move.
    pub(crate) fn time(&self) -> SimTime {
        SimTime {
            state: Arc::clone(&self.state),
        }
    }

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
        let holds = |text: &str| fs::read_to_string(&path).is_ok_and(|held| held.contains(text));
        if state.late.is_some()
            && path.ends_with("log.jsonl")
            && holds(r#""request":{"kind":"ops"}"#)
        {
            state.late_armed = true;
        }
        let reached = |record: &Synced| match *record {
            Synced::Seq(record) => lasts(record),
            Synced::Holding(file, text) => path.ends_with(file) && holds(text),
        };
        let slow = state.slow.pop_front_if(|slow| reached(&slow.record));
        let awaited = (state.held.take_if(|(record, _)| reached(record))).map(|(_, logged)| logged);
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
        let failed = state.failed.take_if(|record| reached(record)).is_some() || file;
        let first = state.first.clone();
        // Without the lock, so that the run's other threads read the clock as they hand the loop
        // its inputs.
        drop(state);
        let waited = awaited.map_or_else(String::new, |logged| {
            await_logged(&path, logged);
            format!(", held until the run log held {logged}")
        });
        let held = slow.map_or_else(String::new, |SlowSync { queued, at, .. }| {
            #[expect(clippy::expect_used, reason = "a test's bug fails the test")]
            first
                .expect("a slow sync of the run log comes once the run's clock has made an inbox")
                .fill(
                    queued,
                    "the test to make every input the slow sync waits on come into the loop's \
                     inbox, which the host takes to be the inbox the clock made first",
                );
            stdx::unpoisoned(self.state.lock()).move_to(at);
            format!(", held until the loop's inbox held {queued}, to {at:?}")
        });
        let outcome = if failed { ", failed" } else { "" };
        if let Some(journal) = &self.journal {
            journal.tell(format!(
                "{kind} {}{through}{waited}{held}{outcome}",
                path.display()
            ));
        }
        if failed {
            return Err(io::Error::other("the simulated host's sync failed"));
        }
        Ok(())
    }
}

// Waits until the run log of the record that holds `path`, in the nearest directory above it that
// holds one, holds `logged` in a line.
fn await_logged(path: &Path, logged: &str) {
    let record = path.ancestors().find(|dir| dir.join("log.jsonl").is_file());
    #[expect(clippy::expect_used, reason = "a test's bug fails the test")]
    crate::sim::logged(
        record.expect("a held sync is of a file in a run's record"),
        logged,
    );
}

fn last_record(path: &Path) -> io::Result<Option<u64>> {
    if path.file_name() != Some("log.jsonl".as_ref()) {
        return Ok(None);
    }
    let records = fs::read_to_string(path)?.lines().count();
    Ok(u64::try_from(records)
        .ok()
        .and_then(|records| records.checked_sub(1)))
}

impl State {
    fn move_to(&mut self, at: Duration) {
        assert!(
            at >= self.now,
            "the simulated clock was moved back, from {:?} to {at:?}: expected a script that moves \
             it only forward",
            self.now
        );
        self.now = at;
    }
}

/// A simulated host's clock, as the simulated provider and its sandboxes read and move it.
#[derive(Clone)]
pub(crate) struct SimTime {
    state: Arc<Mutex<State>>,
}

impl SimTime {
    pub(crate) fn now(&self) -> Duration {
        stdx::unpoisoned(self.state.lock()).now
    }

    /// Moves the clock to `at`, unless it is past it already.
    pub(crate) fn reach(&self, at: Duration) {
        let mut state = stdx::unpoisoned(self.state.lock());
        state.now = state.now.max(at);
    }

    /// Waits until the clock reaches `at`, which only another thread can move it to.
    pub(crate) fn wait_until(&self, at: Duration) {
        let waited = Instant::now();
        while self.now() < at {
            assert!(
                waited.elapsed() < WATCHDOG,
                "a wait on the simulated clock for {at:?} lasted {WATCHDOG:?}: expected the test \
                 to move the clock there"
            );
            std::thread::sleep(LOOK);
        }
    }

    /// The time since the run started that `unix`, a time by the machine's clock that the run
    /// gave, names.
    pub(crate) fn of_unix(unix: Duration) -> Duration {
        unix.saturating_sub(EPOCH)
    }
}

#[derive(Clone)]
pub(crate) struct SimClock {
    state: Arc<Mutex<State>>,
}

impl Clock for SimClock {
    type Inbox<T: Send + 'static> = SimInbox<T>;

    fn now(&self) -> Duration {
        stdx::unpoisoned(self.state.lock()).now
    }

    fn unix(&self) -> SystemTime {
        UNIX_EPOCH + EPOCH
    }

    // A move of the clock ends no wait on the machine's timers, which looks again this often.
    fn look(&self) -> Option<Duration> {
        Some(LOOK)
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

pub(crate) struct SimInbox<T> {
    clock: SimClock,
    queue: Arc<Mutex<Queue<T>>>,
    // Kept, so that the inbox never disconnects, whoever else holds a sender.
    sender: Sender<T>,
    then: usize,
}

struct Queue<T> {
    receiver: Receiver<T>,
    behind: VecDeque<T>,
}

trait Fill: Send + Sync {
    fn fill(&self, queued: usize, expected: &str);
}

impl<T: Send> Fill for Mutex<Queue<T>> {
    fn fill(&self, queued: usize, expected: &str) {
        // A worker syncing its record may be the sender that the loop awaits under this lock.
        if queued == 0 {
            return;
        }
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
        self.next(Some(until))
    }

    fn take(&mut self) -> Option<T> {
        self.next(None)
    }
}

impl<T: Send> SimInbox<T> {
    // Takes the next input: waiting for one if the late take says so, else until the clock
    // reaches `until`, if given, and then taking what the inbox holds. So a move of the clock
    // ends the wait, as Kubernetes' `FakeClock` fires each waiter whose time has passed:
    // https://github.com/kubernetes/utils/blob/ca04ba58ecab621c984998618739fbafa14398ad/clock/testing/fake_clock.go#L175-L222
    fn next(&mut self, until: Option<Duration>) -> Option<T> {
        let held = self.then > 0 || self.during_setup();
        self.then = self.then.saturating_sub(1);
        let mut queue = stdx::unpoisoned(self.queue.lock());
        let input = match queue.behind.pop_front() {
            Some(input) => input,
            None if held => queue.received(SCRIPTED),
            None => {
                let waited = Instant::now();
                loop {
                    let due = until.is_none_or(|until| self.clock.now() >= until);
                    let next = if due {
                        queue.receiver.try_recv().map_err(|error| match error {
                            TryRecvError::Empty => RecvTimeoutError::Timeout,
                            TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
                        })
                    } else {
                        queue.receiver.recv_timeout(LOOK)
                    };
                    match next {
                        Ok(input) => break input,
                        Err(RecvTimeoutError::Timeout) if due => return None,
                        Err(RecvTimeoutError::Timeout) => assert!(
                            waited.elapsed() < WATCHDOG,
                            "the simulated host waited {WATCHDOG:?} for an input or a move of \
                             its clock that never came: expected {SCRIPTED}"
                        ),
                        Err(RecvTimeoutError::Disconnected) => disconnected(),
                    }
                }
            }
        };
        drop(queue);
        Some(self.late(input))
    }

    fn late(&mut self, input: T) -> T {
        let mut state = stdx::unpoisoned(self.clock.state.lock());
        if !state.late_armed {
            return input;
        }
        let late = state.late.take();
        let after = state.after_take.take();
        drop(state);
        let Some(LateTake { behind, at, then }) = late else {
            return input;
        };
        self.then = then;
        // Without the clock's lock, so that the run's other threads read the clock as they wait.
        self.queue.fill(behind, SCRIPTED);
        stdx::unpoisoned(self.clock.state.lock()).move_to(at);
        if let Some(after) = after {
            after();
        }
        input
    }
}

impl<T> SimInbox<T> {
    // Whether a late take during setup, which waits for its input whatever the time, is next.
    fn during_setup(&self) -> bool {
        let state = stdx::unpoisoned(self.clock.state.lock());
        state.late_armed && state.after_take.is_some() && state.late.is_some()
    }
}

impl<T> Queue<T> {
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
