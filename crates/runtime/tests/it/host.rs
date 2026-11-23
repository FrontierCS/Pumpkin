//! A simulated host, [`SimHost`], whose run's loop runs in a simulated world (`World`): its clock
//! moves only as its script says, so that a test decides how long a sync of the run's record
//! lasts, whether it fails, and what time the run reads then, with no real waiting. Its clock only
//! moves forward. It tells a [`Journal`] each sync, in order with what the simulated provider
//! tells it.

use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use runtime::{Clock, Durable, Host, Inbox};
use stdx::io::Backend;
use test_support::world::{Peer, Script, Synced as WorldSynced, World};

use crate::sim::Journal;

// How long a wait of a run's thread on the clock blocks between its looks at it: the threads of
// calls, leases and shared roles, which wait on their own inboxes.
const LOOK: Duration = Duration::from_millis(10);

/// A host whose clock reads 0 from the run's start and never moves by itself: a [`SlowSync`], a
/// call of [`SimHost::moving_to`]'s function or the simulated provider ([`SimTime`]) moves it. A
/// sync makes nothing last, and succeeds unless the script fails it.
pub(crate) struct SimHost {
    world: World,
    journal: Option<Journal>,
}

/// The first sync that makes `record` last, of the run log or another file of the record, lasts
/// until `queued` more inputs have reached the loop, and ends at `at`, as the machine's disk may
/// hold a sync while other threads hand the loop what they observed.
#[derive(Clone, Copy)]
pub(crate) struct SlowSync {
    pub(crate) record: Synced,
    pub(crate) queued: usize,
    pub(crate) at: Duration,
}

/// What a sync makes last.
#[derive(Clone, Copy)]
pub(crate) enum Synced {
    /// The run log's record of that `seq`, with the records before it.
    Seq(u64),
    /// A file whose path ends with the first, as it holds the second, such as a sandbox's
    /// `sandbox.json` once it records the delete.
    Holding(&'static str, &'static str),
}

impl Synced {
    fn reached(self, path: &Path) -> bool {
        match self {
            Self::Seq(record) => last_record(path).is_some_and(|last| last >= record),
            Self::Holding(file, text) => {
                path.ends_with(file)
                    && fs::read_to_string(path).is_ok_and(|held| held.contains(text))
            }
        }
    }
}

impl SimHost {
    pub(crate) fn new() -> Self {
        Self {
            world: World::new(),
            journal: None,
        }
    }

    pub(crate) fn slow_sync(self, slow: SlowSync) -> Self {
        let SlowSync { record, queued, at } = slow;
        self.scripted(record, WorldSynced::Lasts { at, events: queued })
    }

    /// Moves the clock to `at` as the sync that first makes `record` last ends.
    pub(crate) fn moving_after_read(self, record: Synced, at: Duration) -> Self {
        self.scripted(record, WorldSynced::Lasts { at, events: 0 })
    }

    /// Holds the first sync that makes `record` last until the run log holds `logged` in a line,
    /// as the machine may hold one sync while the loop records what others report.
    pub(crate) fn held_sync(self, record: Synced, logged: &'static str) -> Self {
        let held = move |path: &Path| {
            let record = path.ancestors().find(|dir| dir.join("log.jsonl").is_file());
            record.is_some_and(|record| {
                fs::read_to_string(record.join("log.jsonl"))
                    .is_ok_and(|log| log.lines().any(|line| line.contains(logged)))
            })
        };
        self.scripted(record, WorldSynced::Held(Box::new(held)))
    }

    /// Fails the first sync that makes `record` last.
    pub(crate) fn failed_sync(self, record: Synced) -> Self {
        self.scripted(record, WorldSynced::Fails)
    }

    fn scripted(self, record: Synced, synced: WorldSynced) -> Self {
        self.world.script(Script {
            matches: Box::new(move |path| record.reached(path)),
            synced,
        });
        self
    }

    /// Returns a function that moves the clock to `at` each time it is called.
    pub(crate) fn moving_to(&self, at: Duration) -> impl Fn() + Send + Sync + 'static {
        let world = self.world.clone();
        move || world.reach(at)
    }

    /// The clock, for the simulated provider and its sandboxes to read and move.
    pub(crate) fn time(&self) -> SimTime {
        SimTime {
            world: self.world.clone(),
        }
    }

    pub(crate) fn journaled(mut self, journal: &Journal) -> Self {
        let told = journal.clone();
        self.world.observe(Box::new(move |path: &Path, durable| {
            told.tell(synced(path, durable));
        }));
        self.journal = Some(journal.clone());
        self
    }
}

impl Host for SimHost {
    type Clock = SimClock;

    fn start(&self) -> SimClock {
        SimClock {
            world: self.world.clone(),
        }
    }

    fn backend(&self) -> io::Result<Box<dyn Backend>> {
        Ok(self.world.backend())
    }

    fn sync(&self, file: &File, durable: Durable) -> io::Result<()> {
        if let Some(journal) = &self.journal {
            // A file knows no path of its own, but the kernel's table of the process's files does.
            let path = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
            journal.tell(synced(&path, durable));
        }
        Ok(())
    }
}

// What the journal tells of a sync of `path`.
fn synced(path: &Path, durable: Durable) -> String {
    let kind = match durable {
        Durable::Data => "fdatasync",
        Durable::All => "fsync",
    };
    let through =
        last_record(path).map_or_else(String::new, |last| format!(", through record {last}"));
    format!("{kind} {}{through}", path.display())
}

fn last_record(path: &Path) -> Option<u64> {
    if path.file_name() != Some("log.jsonl".as_ref()) {
        return None;
    }
    let records = fs::read_to_string(path).ok()?.lines().count();
    u64::try_from(records)
        .ok()
        .and_then(|records| records.checked_sub(1))
}

/// A simulated host's clock, as the simulated provider and its sandboxes read and move it.
#[derive(Clone)]
pub(crate) struct SimTime {
    world: World,
}

impl SimTime {
    pub(crate) fn now(&self) -> Duration {
        self.world.now()
    }

    /// Moves the clock to `at`, unless it is past it already.
    pub(crate) fn reach(&self, at: Duration) {
        self.world.reach(at);
    }

    /// Waits until the clock reaches `at`, which only another thread can move it to.
    pub(crate) fn wait_until(&self, at: Duration) {
        self.world.wait_until(at);
    }

    /// The time on this clock that `unix`, a time by the machine's clock that the run gave as the
    /// time left from now, names: none past now if it has passed.
    pub(crate) fn of_unix(&self, unix: Duration) -> Duration {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        self.now() + unix.saturating_sub(now)
    }

    /// The Unix time that the clock reads now.
    pub(crate) fn unix(&self) -> SystemTime {
        self.world.realtime()
    }

    /// Connects to the socket at `path`, as a sandbox in the world does.
    pub(crate) fn connect(&self, path: &Path) -> io::Result<Peer> {
        self.world.connect(path)
    }
}

#[derive(Clone)]
pub(crate) struct SimClock {
    world: World,
}

impl stdx::time::Clock for SimClock {
    fn now(&self) -> Duration {
        self.world.now()
    }

    fn realtime(&self) -> SystemTime {
        self.world.realtime()
    }

    // A move of the clock ends no wait on the machine's timers, which looks again this often.
    fn look(&self) -> Option<Duration> {
        Some(LOOK)
    }
}

impl Clock for SimClock {
    type Inbox<T: Send + 'static> = SimInbox<T>;

    fn inbox<T: Send + 'static>(&self) -> SimInbox<T> {
        let (sender, receiver) = mpsc::channel();
        SimInbox {
            world: self.world.clone(),
            receiver,
            sender,
        }
    }
}

pub(crate) struct SimInbox<T> {
    world: World,
    receiver: Receiver<T>,
    // Kept, so that the inbox never disconnects, whoever else holds a sender.
    sender: Sender<T>,
}

impl<T: Send> Inbox<T> for SimInbox<T> {
    fn sender(&self) -> Sender<T> {
        self.sender.clone()
    }

    fn wait(&mut self, until: Duration) -> Option<T> {
        loop {
            if self.world.now() >= until {
                return self.take();
            }
            match self.receiver.recv_timeout(LOOK) {
                Ok(input) => return Some(input),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => disconnected(),
            }
        }
    }

    fn take(&mut self) -> Option<T> {
        match self.receiver.try_recv() {
            Ok(input) => Some(input),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => disconnected(),
        }
    }
}

fn disconnected() -> ! {
    unreachable!(
        "an inbox disconnected, though it holds a sender of its own, so its channel can't \
         disconnect"
    )
}
