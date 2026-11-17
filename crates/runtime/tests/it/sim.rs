//! The simulated provider, `Sim`, which passes the suite that every provider passes
//! ([`suite`](test_support::suite)). Each sandbox is a thread of the test process.
//!
//! A sandbox says hello to the gateway as `sandboxd` does, dialing it at once, or for a provider
//! made [listening](Sim::listening) waiting for its dial behind the stand-in TLS edge, and keeps
//! its files in memory. The program of a command that a run starts in it scripts the command:
//! - `exit <code>`; `stall` (the sandbox stops answering); `crash` (its connection ends);
//! - `print <words>` and `fill <n> <words>` (`n` `x`es first) leave their text as the command's
//!   stdout, and `list <dir>` leaves a listing of what was put under `dir`;
//! - `judge <path> [<file>]` scores the payload put at `path` by its length, except `slow` (it
//!   never ends) and `large` (a result over its limit), and puts its result at `file` too, if
//!   named; with no payload it exits 1;
//! - `ops <step>...` plays its steps (`step`) in turn, one op at a time, and exits 0 after the
//!   last; any other program runs until it is killed.
//!
//! A test chooses a fault by naming a sentinel, such as `HUNG`, as the sandboxd's path or, through
//! [`image`], as the image. What a run acts on may go to a [`Journal`], in order with what other
//! parts of the test's simulated world tell it.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::num::{NonZeroU32, NonZeroU64};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use provider::{
    Console, Create, Created, Creator, ImageRef, Listed, Meeting, Provider, Ready, Reservation,
    Role, RunTag, SandboxId, SandboxdRef, Sweep,
};
use schema::ir::{AbsolutePath, Argv, CpuModel, Image, Mode, Name, Sha256Digest};
use schema::sandbox::op::{
    JobState, Last, LeaseStatusReply, OpCall, OpReply, SessionOp, StatusReply, TransferStatus,
};
use schema::sandbox::{
    Endpoint, Entry, ExitStatus, FromSandbox, HelloReply, Launch, Listening, PROTOCOL_VERSION,
    RESULT_MAX_BYTES, RegistrationGrant, RelativePath, Root, SOCKET_NAME, SocketDial, TcpDial,
    ToSandbox, dir_mode, file_mode, frame, log_dir, out_dir, pumpkin_dir,
};
use schema::workflow::{Class, Offers, UsdPerHour};
use serde_json::{Map, Value, json};
use stdx::Deadline;
use test_support::suite::{Locals, Subject};

/// A sandbox that never dials, and ends at once.
pub(crate) const MUTE: &str = "/sim/mute";

/// A sandbox that never dials, and lasts until it is deleted.
pub(crate) const HUNG: &str = "/sim/hung";

/// A sandbox that never dials, and that its provider's list leaves out once it has ended.
pub(crate) const VANISHING: &str = "/sim/vanishing";

/// A sandbox that never dials, and whose provider's list fails while it exists.
pub(crate) const UNLISTABLE: &str = "/sim/unlistable";

/// A create that fails without naming the sandbox it made, which never dials.
pub(crate) const LOST: &str = "/sim/lost";

/// A sandbox whose hello names a protocol version later than the gateway's.
pub(crate) const LATER: &str = "/sim/later";

/// A create that takes two seconds of real time, or until its deadline if that comes first, in
/// which case it fails, leaving the sandbox for the run's list to find, which, where it listens,
/// ends itself as no gateway claims it.
pub(crate) const SLOW: &str = "/sim/slow";

// Far longer than a run takes to act on what its agent does as the create begins, or a `sandboxd`
// takes to dial, and far shorter than the startup of the providers whose tests use it.
const SLOW_CREATE: Duration = Duration::from_secs(2);

/// The class with a CPU model and a short startup.
pub(crate) const BRIEF: &str = "brief";

// Far longer than a simulated sandbox takes to start, so that a busy machine makes a start that
// succeeds wait longer, never fail.
const BRIEF_STARTUP: u64 = 2;

const AWAIT_POLL: Duration = Duration::from_millis(10);

// The file that each collected directory holds, unless a put wrote one of its name.
const COLLECTED: &str = "collected";

/// A `sandboxd` or an image that publishing fails on.
pub(crate) const UNPUBLISHED: &str = "/sim/unpublished";

/// A sandbox that every delete of fails.
pub(crate) const UNDELETABLE: &str = "/sim/undeletable";

/// A sandbox whose first delete fails, and the next deletes it, as a provider's API may fail a
/// call once.
pub(crate) const REFUSED_ONCE: &str = "/sim/refused-once";

/// A sandbox whose delete takes a second of real time.
pub(crate) const DAWDLING: &str = "/sim/dawdling";

/// A sandbox where a process changes `out` until a freeze kills it, so that a collect of `out`
/// before the freeze fails, as `sandboxd`'s of a tree that changes as it is collected does.
pub(crate) const SCRIBBLED: &str = "/sim/scribbled";

/// A sandbox where a process outlives every freeze, which fails, as `sandboxd`'s does when a kill
/// fails.
pub(crate) const UNFREEZABLE: &str = "/sim/unfreezable";

/// A sandbox whose delete waits until each other sandbox of its run is deleted, and no other
/// delete is under way, as a platform may finish a delete that began later first.
pub(crate) const LAST: &str = "/sim/last";

// How long a delete of a `LAST` sandbox waits before the test fails instead of hanging.
const LAST_WAITS: Duration = Duration::from_secs(60);

// Far longer than a run takes to collect what a call's sandbox holds and delete it, and far
// shorter than the bounds of the runs whose tests use it.
const DAWDLE: Duration = Duration::from_secs(1);

// A platform's sandbox ends some time after it is told to, so a delete that doesn't wait for its
// end returns while it runs, which the suite then sees (`Subject::held`): far longer than the
// suite takes to look after a delete returns, and far shorter than any test's bounds.
const ENDING: Duration = Duration::from_millis(100);

/// A simulated provider, whose clones from [`Subject::connect`] share its sandboxes.
pub(crate) struct Sim {
    platform: &'static str,
    ready: Ready,
    reach: Reach,
    // `Provider`'s methods take `&self`, and it is `Sync`, so what they change is behind a lock,
    // which each instance that `Subject::connect` makes shares, as a platform's instances share
    // its sandboxes.
    state: Arc<Mutex<State>>,
    journal: Option<Journal>,
}

#[derive(Clone, Copy)]
enum Reach {
    Local,
    Private,
    Listen,
}

#[derive(Clone, Default)]
/// What the simulated provider and host tell, in the order it happened, so that a test reads how
/// their events interleaved.
pub(crate) struct Journal {
    // Shared, as each part tells it from its own thread, and the test reads it.
    entries: Arc<Mutex<Vec<String>>>,
}

impl Journal {
    pub(crate) fn tell(&self, entry: String) {
        stdx::unpoisoned(self.entries.lock()).push(entry);
    }

    pub(crate) fn entries(&self) -> Vec<String> {
        stdx::unpoisoned(self.entries.lock()).clone()
    }
}

struct State {
    next: u64,
    sandboxes: BTreeMap<String, Sandbox>,
    deleted: Vec<Deleted>,
    deleting: usize,
    modes: Vec<u32>,
    models: Vec<bool>,
    classes: Vec<Name>,
    published: Vec<String>,
    cpus: u64,
    reserved: Vec<Reserved>,
    /// The record of the run whose sandboxes these are, once `Sim::records` names it.
    record: Option<PathBuf>,
}

struct Reserved {
    run: RunTag,
    roles: Vec<(Name, NonZeroU32)>,
    cpus: u64,
    live: Weak<()>,
}

struct Sandbox {
    run: RunTag,
    refusals: u32,
    linger: Duration,
    last: bool,
    listing: Listing,
    // Shared with the sandbox's thread, which prints to its console and hands over its
    // connection.
    shared: Arc<Mutex<Status>>,
    thread: JoinHandle<()>,
}

impl Sandbox {
    fn of(&self, run: &RunTag) -> bool {
        self.run.as_str() == run.as_str()
    }

    fn ended(&self) -> bool {
        self.thread.is_finished() && !status(&self.shared).stalled
    }
}

struct Deleted {
    id: String,
    run: RunTag,
    shared: Weak<Mutex<Status>>,
}

enum Listing {
    Shown,
    LeftOutOnceEnded,
    Failing,
}

#[derive(Default)]
struct Status {
    console: String,
    connection: Option<Connection>,
    stalled: bool,
    deleted: bool,
    freezing: Freezing,
    journal: Option<Journal>,
    record: Option<PathBuf>,
}

/// What a sandbox's freeze meets.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Freezing {
    /// Processes that it kills, none of which change what is collected.
    #[default]
    Killed,
    /// A process that changes `out` until it is killed (`SCRIBBLED`).
    Scribbler,
    /// A process that it fails to kill (`UNFREEZABLE`).
    Unkillable,
}

impl Freezing {
    /// The answer to a freeze on `stream`, and whether the sandbox is frozen by it.
    fn freeze(self, stream: u32) -> (FromSandbox, bool) {
        if self == Self::Unkillable {
            let message = "the sandbox's processes couldn't be killed: Operation not permitted \
                           (os error 1)"
                .to_owned();
            return (FromSandbox::Failed { stream, message }, false);
        }
        (FromSandbox::Done { stream }, true)
    }

    /// Whether the tree at `path` changes as it is collected, unless the sandbox is `frozen`.
    fn changes(self, path: &AbsolutePath, frozen: bool) -> bool {
        self == Self::Scribbler && !frozen && *path == pumpkin_dir().join(&out_dir())
    }
}

impl Status {
    fn new(journal: Option<Journal>, record: Option<PathBuf>) -> Self {
        Self {
            journal,
            record,
            ..Self::default()
        }
    }
}

impl Sim {
    pub(crate) fn new() -> Self {
        // Far longer than a thread takes to start and dial, so that a busy machine makes a start
        // wait longer, never fail.
        Self::with_startup(30)
    }

    pub(crate) fn with_startup(seconds: u64) -> Self {
        Self {
            platform: "sim",
            ready: ready(NonZeroU64::new(seconds).unwrap()),
            reach: Reach::Local,
            state: Arc::new(Mutex::new(State {
                next: 1,
                sandboxes: BTreeMap::new(),
                deleted: Vec::new(),
                deleting: 0,
                modes: Vec::new(),
                models: Vec::new(),
                classes: Vec::new(),
                published: Vec::new(),
                cpus: 1 << 10,
                reserved: Vec::new(),
                record: None,
            })),
            journal: None,
        }
    }

    pub(crate) fn priced(mut self, usd_per_hour: f64) -> Self {
        let sim = Name::try_from("sim".to_owned()).unwrap();
        let class = self.ready.classes.get_mut(&sim).unwrap();
        class.usd_per_hour = Some(UsdPerHour::try_from(usd_per_hour).unwrap());
        self
    }

    #[must_use]
    pub(crate) fn journaled(mut self, journal: &Journal) -> Self {
        self.journal = Some(journal.clone());
        self
    }

    pub(crate) fn journal(&self) -> Option<&Journal> {
        self.journal.as_ref()
    }

    /// Names `record` as the record of the run whose sandboxes it creates, whose log `await_log`
    /// reads.
    pub(crate) fn records(&self, record: &Path) {
        self.state().record = Some(record.to_owned());
    }

    fn holds(&self, create: &Create) -> Result<(), String> {
        let reserved = (self.state().reserved.iter()).any(|reserved| {
            reserved.live.strong_count() > 0
                && reserved.run.as_str() == create.run.as_str()
                && (reserved.roles.iter()).any(|(role, instances)| {
                    *role == create.role.name && create.instance < instances.get()
                })
        });
        if reserved {
            return Ok(());
        }
        Err(format!(
            "run {} holds no simulated CPUs for instance {} of role `{}`: reserve them before its \
             creates",
            create.run.as_str(),
            create.instance,
            create.role.name
        ))
    }

    fn tell(&self, entry: impl FnOnce() -> String) {
        if let Some(journal) = &self.journal {
            journal.tell(entry());
        }
    }

    #[must_use]
    /// Names the platform, which prefixes the references that it publishes and the only ones it
    /// takes.
    pub(crate) fn named(self, platform: &'static str) -> Self {
        Self { platform, ..self }
    }

    #[must_use]
    pub(crate) fn private(self) -> Self {
        Self {
            reach: Reach::Private,
            ..self
        }
    }

    #[must_use]
    /// Sets the simulated CPUs that reservations share, 1024 at first.
    pub(crate) fn with_cpus(self, cpus: u64) -> Self {
        self.state().cpus = cpus;
        self
    }

    #[must_use]
    pub(crate) fn offering_memory(mut self, memory_bytes: u64) -> Self {
        for class in self.ready.classes.values_mut() {
            class.offers.memory_bytes = NonZeroU64::new(memory_bytes).unwrap();
        }
        self
    }

    #[must_use]
    pub(crate) fn listening(self) -> Self {
        Self {
            reach: Reach::Listen,
            ..self
        }
    }

    /// The mode of the sockets directory at each create that met by a socket.
    pub(crate) fn modes(&self) -> Vec<u32> {
        self.state().modes.clone()
    }

    pub(crate) fn models(&self) -> Vec<bool> {
        self.state().models.clone()
    }

    pub(crate) fn classes(&self) -> Vec<Name> {
        self.state().classes.clone()
    }

    pub(crate) fn published(&self) -> Vec<String> {
        self.state().published.clone()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        stdx::unpoisoned(self.state.lock())
    }

    fn own<'r>(&self, what: &str, reference: &'r str) -> Result<&'r str, String> {
        let rest = (reference.strip_prefix(self.platform)).and_then(|rest| rest.strip_prefix(':'));
        rest.ok_or_else(|| {
            format!(
                "{what} `{reference}` is no reference of platform `{}`: name one that its \
                 provider published",
                self.platform
            )
        })
    }
}

fn ready(startup_seconds: NonZeroU64) -> Ready {
    let class = |name: &str, startup_seconds, cpu_model| {
        let offers = Offers {
            cpu_millis: NonZeroU64::new(1 << 20).unwrap(),
            memory_bytes: NonZeroU64::new(1 << 50).unwrap(),
            gpu: None,
            cpu_features: BTreeSet::new(),
            cpu_model,
        };
        let class = Class {
            offers,
            startup_seconds,
            usd_per_hour: None,
        };
        (Name::try_from(name.to_owned()).unwrap(), class)
    };
    let model = Some(CpuModel::try_from(BRIEF.to_owned()).unwrap());
    let brief = NonZeroU64::new(BRIEF_STARTUP).unwrap();
    Ready {
        report: json!({ "provider": "sim" }),
        classes: BTreeMap::from([
            class("sim", startup_seconds, None),
            class(BRIEF, brief, model),
        ]),
    }
}

impl Sweep for Sim {
    fn delete(&self, run: &RunTag, ids: &[SandboxId]) -> Result<(), String> {
        let (mut deleted, mut failures) = (Vec::new(), Vec::new());
        let mut state = self.state();
        let named = |id: &str| ids.iter().any(|ours| ours.as_str() == id);
        let waits = |state: &State| {
            let last = (state.sandboxes.iter()).any(|(id, sandbox)| named(id) && sandbox.last);
            let others =
                (state.sandboxes.iter()).any(|(id, sandbox)| sandbox.of(run) && !named(id));
            last && (others || state.deleting > 0)
        };
        let waited = Deadline::after(LAST_WAITS);
        while waits(&state) {
            drop(state);
            assert!(
                waited.remaining().is_some(),
                "a delete of a `LAST` sandbox waited {LAST_WAITS:?} for the run's others: expected \
                 the test to delete each"
            );
            thread::sleep(AWAIT_POLL);
            state = self.state();
        }
        state.deleting += 1;
        for id in ids.iter().map(SandboxId::as_str) {
            let found = state.sandboxes.get_mut(id);
            match found.map(|sandbox| (sandbox.of(run), sandbox.refusals > 0, sandbox)) {
                None => {}
                Some((false, _, _)) => failures.push(format!(
                    "{id} is another run's sandbox, not run {}'s, so the simulated provider left \
                     it: name only the run's own",
                    run.as_str()
                )),
                Some((true, true, sandbox)) => {
                    if sandbox.refusals != u32::MAX {
                        sandbox.refusals -= 1;
                    }
                    failures.push(format!(
                        "{id} can't be deleted by the simulated provider: create one with another \
                         `sandboxd`"
                    ));
                }
                Some((true, false, _)) => deleted.extend(state.sandboxes.remove_entry(id)),
            }
        }
        state
            .deleted
            .extend(deleted.iter().map(|(id, sandbox)| Deleted {
                id: id.clone(),
                run: sandbox.run.clone(),
                shared: Arc::downgrade(&sandbox.shared),
            }));
        // Each thread ends once told, and joining it waits for nothing the lock holds.
        drop(state);
        for (id, sandbox) in deleted {
            self.tell(|| format!("deleted {id}"));
            stop(sandbox);
        }
        self.state().deleting -= 1;
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }

    fn list(&self, run: &RunTag) -> Result<Vec<Listed>, String> {
        let state = self.state();
        let ours = (state.sandboxes.iter()).filter(|(_, sandbox)| sandbox.of(run));
        let mut listed = Vec::new();
        for (id, sandbox) in ours {
            let ended = sandbox.ended();
            match sandbox.listing {
                Listing::Failing => {
                    return Err(format!(
                        "the simulated list failed while {id} lasts: delete it, or create one \
                         with another `sandboxd`"
                    ));
                }
                Listing::LeftOutOnceEnded if ended => {}
                Listing::Shown | Listing::LeftOutOnceEnded => listed.push(Listed {
                    id: SandboxId::from(id.clone()),
                    ended,
                }),
            }
        }
        Ok(listed)
    }
}

impl Provider for Sim {
    fn ready(&self) -> &Ready {
        &self.ready
    }

    fn reserve(&self, run: &RunTag, phases: &[Vec<Role>]) -> Result<Reservation, String> {
        let needs = |phase: &Vec<Role>| -> u64 {
            (phase.iter())
                .map(|role| {
                    let cpus = role.resources.cpu_millis.get().div_ceil(1000);
                    cpus * u64::from(role.instances.get())
                })
                .sum()
        };
        // A phase starts only once the one before has ended, so the run holds the most of any.
        let needed = phases.iter().map(needs).max().unwrap_or(0);
        let mut roles: Vec<(Name, NonZeroU32)> = phases
            .iter()
            .flatten()
            .map(|role| (role.name.clone(), role.instances))
            .collect();
        roles.sort();
        let count = roles.len();
        roles.dedup_by(|(a, _), (b, _)| a == b);
        assert!(roles.len() == count, "a role is in more than one phase");
        let mut state = self.state();
        state
            .reserved
            .retain(|reserved| reserved.live.strong_count() > 0);
        let held: u64 = state.reserved.iter().map(|reserved| reserved.cpus).sum();
        if held.saturating_add(needed) > state.cpus {
            return Err(format!(
                "the run needs {needed} simulated CPUs, and {held} of the {} are held: run it \
                 once others end",
                state.cpus
            ));
        }
        let live = Arc::new(());
        state.reserved.push(Reserved {
            run: run.clone(),
            roles,
            cpus: needed,
            live: Arc::downgrade(&live),
        });
        Ok(Reservation::new(live))
    }

    fn publish_image(&self, image: &Image) -> Result<ImageRef, String> {
        if *image == self::image(UNPUBLISHED) {
            return Err(format!(
                "{image} can't be published by the simulated provider: name another image"
            ));
        }
        self.state().published.push(format!("image {image}"));
        Ok(ImageRef::from(format!("{}:{image}", self.platform)))
    }

    fn publish_sandboxd(&self, path: &Path) -> Result<SandboxdRef, String> {
        if path.as_os_str() == UNPUBLISHED {
            return Err(format!(
                "{UNPUBLISHED} can't be published by the simulated provider: name another path"
            ));
        }
        let path = path.display();
        self.state().published.push(format!("sandboxd {path}"));
        Ok(SandboxdRef::from(format!("{}:{path}", self.platform)))
    }

    fn meeting(&self) -> Meeting<'_> {
        match self.reach {
            Reach::Local => Meeting::Local(self),
            Reach::Private => Meeting::Private(self),
            Reach::Listen => Meeting::Listen(self),
        }
    }

    fn console(&self, run: &RunTag, id: &SandboxId) -> Result<Console, String> {
        self.console_of(run, id)
    }
}

impl Creator<SocketDial> for Sim {
    fn create(&self, create: &Create, dial: SocketDial) -> Result<Created, String> {
        let gateway = Gateway::Socket(dial.dir.join(SOCKET_NAME));
        self.made(create, (gateway, None, None))
    }
}

impl Creator<TcpDial> for Sim {
    fn create(&self, create: &Create, dial: TcpDial) -> Result<Created, String> {
        let TcpDial { gateway, grant } = dial;
        self.made(create, (Gateway::Tcp(gateway), Some(grant), None))
    }
}

impl Creator<Listening, (Created, Endpoint)> for Sim {
    fn create(&self, create: &Create, dial: Listening) -> Result<(Created, Endpoint), String> {
        let loopback = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|error| {
            format!("the simulated sandbox can't listen on the loopback: {error}")
        })?;
        let port = loopback.local_addr().unwrap().port();
        let endpoint = Endpoint::new(test_support::edge::NAME.to_owned(), port).unwrap();
        let gateway = Gateway::Listen(loopback, Deadline::at_unix(create.by.since_epoch()));
        let created = self.made(create, (gateway, None, Some(dial.grant)))?;
        Ok((created, endpoint))
    }
}

impl Sim {
    // A sandbox, a thread of this process, ends with it, so the hard end needs nothing more; and
    // the name needs no use, as each sandbox is created once.
    fn made(&self, create: &Create, met: Met) -> Result<Created, String> {
        let by = Deadline::at_unix(create.by.since_epoch());
        if by.remaining().is_none() {
            let passed =
                "the simulated create's deadline had passed: expected time left to make it";
            return Err(passed.to_owned());
        }
        self.holds(create)?;
        let path = self.own("the `sandboxd`", create.sandboxd.as_str())?;
        let digest = self.own("the image", create.image.as_str())?;
        let (gateway, presented, awaited) = met;
        let Faults {
            made,
            dialing,
            listing,
            refusals,
            linger,
            last,
            freezing,
        } = Faults::of(|fault: &str| path == fault || digest == image(fault).as_str());
        let (mute, hung) = dialing.silent();
        let hello = if dialing == Dialing::Later {
            json!({ "version": PROTOCOL_VERSION + 1 })
        } else {
            json!({ "version": PROTOCOL_VERSION, "grant": presented })
        };
        let mode = gateway.mode();
        let record = self.state().record.clone();
        let shared = Arc::new(Mutex::new(Status::new(self.journal.clone(), record)));
        // Before its thread starts, so that the console shows it once the create returns, however
        // late the thread runs.
        (status(&shared).console).push_str(if mute { NEVER_DIALS } else { "" });
        status(&shared).freezing = freezing;
        let (ours, model) = (Arc::clone(&shared), create.model);
        let thread = thread::Builder::new()
            .name("sim-sandboxd".to_owned())
            .spawn(move || sandboxd(&gateway, mute, hung, (&hello, awaited), model, &ours))
            .map_err(|error| format!("the sandbox's thread didn't start: {error}"))?;
        let late = made == Made::Slow && {
            thread::sleep(SLOW_CREATE.min(by.remaining().unwrap_or_default()));
            by.remaining().is_none()
        };
        let mut state = self.state();
        let id = format!("sim-{}", state.next);
        state.next += 1;
        state.modes.extend(mode);
        state.models.push(model);
        state.classes.push(create.class.clone());
        let sandbox = Sandbox {
            run: create.run.clone(),
            refusals,
            linger,
            last,
            listing,
            shared,
            thread,
        };
        state.sandboxes.insert(id.clone(), sandbox);
        self.tell(|| format!("created {id}"));
        if made == Made::Lost || late {
            let failed = if made == Made::Lost {
                "failed"
            } else {
                "gave up at its deadline"
            };
            return Err(format!(
                "the simulated create {failed}, and can't name what it made: expected the run's \
                 list to find it"
            ));
        }
        Ok(Created {
            id: SandboxId::from(id),
        })
    }

    fn console_of(&self, run: &RunTag, id: &SandboxId) -> Result<Console, String> {
        let state = self.state();
        let ours = state.sandboxes.get(id.as_str());
        let Some(sandbox) = ours.filter(|sandbox| sandbox.of(run)) else {
            return Err(format!(
                "{} is no sandbox of run {}: name one that the run's list finds",
                id.as_str(),
                run.as_str()
            ));
        };
        let text = status(&sandbox.shared).console.clone();
        Ok(Console { text })
    }
}

enum Gateway {
    Socket(PathBuf),
    Tcp(SocketAddr),
    Listen(TcpListener, Deadline),
}

type Met = (
    Gateway,
    Option<RegistrationGrant>,
    Option<RegistrationGrant>,
);

impl Gateway {
    fn mode(&self) -> Option<u32> {
        match self {
            Self::Socket(socket) => {
                let parent = fs::metadata(socket.parent()?.parent()?).unwrap();
                Some(parent.permissions().mode() & 0o777)
            }
            Self::Tcp(_) | Self::Listen(..) => None,
        }
    }
}

enum Connection {
    Unix(UnixStream),
    Tcp(TcpStream),
    Tls(tls::Stream),
}

impl Connection {
    fn dial(gateway: &Gateway, shared: &Mutex<Status>) -> io::Result<Self> {
        match gateway {
            Gateway::Socket(socket) => UnixStream::connect(socket).map(Self::Unix),
            Gateway::Tcp(address) => TcpStream::connect(address).map(Self::Tcp),
            Gateway::Listen(listener, by) => {
                listener.set_nonblocking(true)?;
                let stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            if status(shared).deleted {
                                return Err(io::Error::other("the sandbox was deleted"));
                            }
                            if by.remaining().is_none() {
                                let given_up = "no gateway claimed the sandbox by its deadline";
                                return Err(io::Error::other(given_up));
                            }
                            thread::sleep(AWAIT_POLL);
                        }
                        Err(error) => return Err(error),
                    }
                };
                stream.set_nonblocking(false)?;
                let by = Deadline::after(Duration::from_secs(60));
                test_support::edge::server()
                    .accept(stream, by)
                    .map(Self::Tls)
            }
        }
    }

    fn try_clone(&self) -> io::Result<Self> {
        match self {
            Self::Unix(stream) => stream.try_clone().map(Self::Unix),
            Self::Tcp(stream) => stream.try_clone().map(Self::Tcp),
            Self::Tls(stream) => stream.try_clone().map(Self::Tls),
        }
    }

    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.shutdown(how),
            Self::Tcp(stream) => stream.shutdown(how),
            // A TLS stream ends both ways at once; no test of a listening sandbox ends only its
            // reads.
            Self::Tls(stream) => stream.shutdown(),
        }
    }
}

impl Read for Connection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.read(buf),
            Self::Tcp(stream) => stream.read(buf),
            Self::Tls(stream) => (&*stream).read(buf),
        }
    }
}

impl Write for Connection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.write(buf),
            Self::Tcp(stream) => stream.write(buf),
            Self::Tls(stream) => (&*stream).write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.flush(),
            Self::Tcp(stream) => stream.flush(),
            Self::Tls(stream) => (&*stream).flush(),
        }
    }
}

/// An image whose digest is `name`'s bytes, which the provider reads as a sentinel.
pub(crate) fn image(name: &str) -> Image {
    let mut bytes = [0; 32];
    bytes[..name.len()].copy_from_slice(name.as_bytes());
    Image::try_from(String::from(Sha256Digest::from(bytes))).unwrap()
}

impl Subject for Sim {
    fn connect(&self) -> Result<Box<dyn Provider + '_>, String> {
        Ok(Box::new(Self {
            platform: self.platform,
            ready: Ready {
                report: self.ready.report.clone(),
                classes: self.ready.classes.clone(),
            },
            reach: self.reach,
            state: Arc::clone(&self.state),
            journal: self.journal.clone(),
        }))
    }

    fn locals(&self, _: &Path) -> Locals {
        Locals {
            image: image("sim"),
            sandboxd: PathBuf::from("/sim/sandboxd"),
            mute: PathBuf::from(MUTE),
            broken: PathBuf::from(LOST),
            unreachable_image: image(UNPUBLISHED),
            unreachable_sandboxd: PathBuf::from(UNPUBLISHED),
        }
    }

    fn stall(&self, id: &SandboxId) {
        let state = self.state();
        status(&state.sandboxes[id.as_str()].shared).stalled = true;
    }

    fn held(&self, run: &RunTag) -> Vec<SandboxId> {
        let state = self.state();
        // An ended sandbox that its list leaves out stands for one the platform has freed, though
        // the provider keeps it until it is deleted, to end its thread.
        let freed = |sandbox: &Sandbox| {
            matches!(sandbox.listing, Listing::LeftOutOnceEnded) && sandbox.ended()
        };
        let held = (state.sandboxes.iter())
            .filter(|(_, sandbox)| sandbox.of(run) && !freed(sandbox))
            .map(|(id, _)| id);
        // A deleted sandbox whose thread still runs stands for one still ending.
        let ending = (state.deleted.iter())
            .filter(|deleted| deleted.run.as_str() == run.as_str())
            .filter(|deleted| deleted.shared.strong_count() > 0)
            .map(|deleted| &deleted.id);
        held.chain(ending)
            .map(|id| SandboxId::from(id.clone()))
            .collect()
    }

    // Its creates refuse a role its run holds no simulated CPUs for.
    fn reserves(&self) -> bool {
        true
    }
}

fn status(shared: &Mutex<Status>) -> MutexGuard<'_, Status> {
    stdx::unpoisoned(shared.lock())
}

struct Faults {
    made: Made,
    dialing: Dialing,
    listing: Listing,
    refusals: u32,
    linger: Duration,
    last: bool,
    freezing: Freezing,
}

#[derive(PartialEq, Eq)]
enum Made {
    Named,
    Lost,
    Slow,
}

#[derive(PartialEq, Eq)]
enum Dialing {
    Hello,
    Later,
    Mute,
    Hung,
}

impl Dialing {
    fn silent(&self) -> (bool, bool) {
        match self {
            Self::Hello | Self::Later => (false, false),
            Self::Mute => (true, false),
            Self::Hung => (true, true),
        }
    }
}

impl Faults {
    fn of(names: impl Fn(&str) -> bool) -> Self {
        let (lost, unlistable, vanishing) = (names(LOST), names(UNLISTABLE), names(VANISHING));
        let listing = if unlistable {
            Listing::Failing
        } else if vanishing {
            Listing::LeftOutOnceEnded
        } else {
            Listing::Shown
        };
        let made = if lost {
            Made::Lost
        } else if names(SLOW) {
            Made::Slow
        } else {
            Made::Named
        };
        let dialing = if unlistable || names(HUNG) {
            Dialing::Hung
        } else if lost || vanishing || names(MUTE) {
            Dialing::Mute
        } else if names(LATER) {
            Dialing::Later
        } else {
            Dialing::Hello
        };
        let refusals = if names(UNDELETABLE) {
            u32::MAX
        } else {
            u32::from(names(REFUSED_ONCE))
        };
        Self {
            made,
            dialing,
            listing,
            refusals,
            linger: if names(DAWDLING) {
                DAWDLE
            } else {
                Duration::ZERO
            },
            last: names(LAST),
            freezing: if names(SCRIBBLED) {
                Freezing::Scribbler
            } else if names(UNFREEZABLE) {
                Freezing::Unkillable
            } else {
                Freezing::Killed
            },
        }
    }
}

fn stop(sandbox: Sandbox) {
    let Sandbox {
        run: _,
        refusals: _,
        linger,
        last: _,
        listing: _,
        shared,
        thread,
    } = sandbox;
    thread::sleep(linger);
    let mut status = status(&shared);
    status.deleted = true;
    if let Some(connection) = status.connection.take() {
        let _ = connection.shutdown(Shutdown::Both);
    }
    drop(status);
    // A thread that hangs waits to be woken once its sandbox is deleted.
    thread.thread().unpark();
    // Only a bug panics the thread, and joining raises its panic here.
    thread.join().unwrap();
}

const NEVER_DIALS: &str = "sandboxd: a simulated one, which never dials\n";

fn sandboxd(
    gateway: &Gateway,
    mute: bool,
    hung: bool,
    hello: (&Value, Option<RegistrationGrant>),
    model: bool,
    shared: &Mutex<Status>,
) {
    if mute {
        // A park can end spuriously, so the deletion is checked again.
        while hung && !status(shared).deleted {
            thread::park();
        }
    } else {
        dial(gateway, hello, model, shared);
    }
    if status(shared).deleted {
        thread::sleep(ENDING);
    }
}

fn dial(
    gateway: &Gateway,
    (hello, awaited): (&Value, Option<RegistrationGrant>),
    model: bool,
    shared: &Mutex<Status>,
) {
    let dialed = Connection::dial(gateway, shared).and_then(|connection| {
        let other = connection.try_clone()?;
        Ok((connection, other))
    });
    let mut status = status(shared);
    match dialed {
        // A sandbox deleted as it dialed has no session to serve.
        Ok(_) if status.deleted => {}
        Ok((mut connection, other)) => {
            status.connection = Some(other);
            drop(status);
            if open(&mut connection, hello, awaited.as_ref()) {
                serve(connection, model, shared);
            }
        }
        Err(error) => {
            let at = match gateway {
                Gateway::Socket(socket) => socket.display().to_string(),
                Gateway::Tcp(address) => address.to_string(),
                Gateway::Listen(listener, _) => listener.local_addr().unwrap().to_string(),
            };
            let failed = format!("sandboxd: connecting to the gateway at {at} failed: {error}");
            status.console.push_str(&failed);
        }
    }
}

fn open(connection: &mut Connection, hello: &Value, awaited: Option<&RegistrationGrant>) -> bool {
    let sent = frame::write(connection, hello, &[]);
    let answer = sent.and_then(|()| frame::read::<HelloReply>(connection));
    // Unlike `sandboxd`, it prints no refusal: a run reads the console of a start that failed as
    // soon as the refusal is sent, so what it found would depend on a race. A connection may also
    // end before the session opens, as a delete ends it.
    matches!(answer, Ok(Some((HelloReply::Accepted { grant }, _))) if grant.as_ref() == awaited)
}

enum Step {
    /// An op, whose payload follows it, and which `less` bounds the remaining seconds of.
    Op {
        call: OpCall,
        payload: Vec<u8>,
        less: Option<u64>,
    },
    /// A `status` or a session's `status` made until its reply gives `state`.
    Await { call: OpCall, state: String },
    /// A model request, whose response's part or end the next step waits for.
    Model {
        model: String,
        chat: bool,
        said: Vec<String>,
        fields: Map<String, Value>,
        wait: Wait,
    },
    /// A wait of that many seconds.
    Sleep(u64),
    /// The `<kind>` of `await_log <kind>`.
    Logged(String),
    /// The command's exit with 0.
    Exit,
    /// The answer to the collects of a `held` directory.
    Release,
    /// A result, `{"raw_score": …}`, of the last number read.
    Result,
    /// The session's end.
    End(End),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    Crash,
    Close,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Wait {
    Part,
    End,
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "each is a fact of the fake command's own, which its steps set apart"
)]
struct Caller {
    stream: u32,
    steps: VecDeque<Step>,
    op: u32,
    less: Option<u64>,
    left: Option<u64>,
    awaiting: Option<(OpCall, String)>,
    exited: bool,
    endpoint: bool,
    model: Option<u32>,
    open: bool,
    waits: Option<Wait>,
    stdout: String,
    read: String,
    result: Option<Vec<u8>>,
    end: Option<End>,
    released: bool,
    /// The run's record, if `Sim::records` named it.
    record: Option<PathBuf>,
}

impl Caller {
    fn new(stream: u32, steps: VecDeque<Step>, endpoint: bool, record: Option<PathBuf>) -> Self {
        Self {
            stream,
            steps,
            op: 0,
            less: None,
            left: None,
            awaiting: None,
            exited: false,
            endpoint,
            model: None,
            open: false,
            waits: None,
            stdout: String::new(),
            read: String::new(),
            result: None,
            end: None,
            released: false,
            record,
        }
    }

    fn answered(&mut self, reply: &OpReply, replies: &mut Vec<(FromSandbox, Vec<u8>)>) -> bool {
        if let OpReply::LeaseStatus(LeaseStatusReply {
            last: Some(Last::Command(command)),
            ..
        }) = reply
        {
            self.read.clone_from(&command.stdout);
        }
        if let OpReply::Ops(ops) = reply {
            let left = ops.remaining_seconds;
            if let Some(less) = self.less
                && self
                    .left
                    .is_none_or(|last| left.saturating_add(less) > last)
            {
                self.steps.clear();
                self.exit(1, replies);
            }
            self.left = Some(left);
        }
        if let Some((call, state)) = self.awaiting.take()
            && !reached(reply, &state)
        {
            thread::sleep(AWAIT_POLL);
            self.make(call.clone(), Vec::new(), None, replies);
            self.awaiting = Some((call, state));
            return false;
        }
        self.next(replies)
    }

    fn next(&mut self, replies: &mut Vec<(FromSandbox, Vec<u8>)>) -> bool {
        loop {
            let (call, payload, less) = match self.steps.pop_front() {
                Some(Step::Op {
                    call,
                    payload,
                    less,
                }) => (call, payload, less),
                Some(Step::Model {
                    model,
                    chat,
                    said,
                    fields,
                    wait,
                }) => {
                    if self.request((&model, chat), &said, fields, replies) {
                        self.waits = Some(wait);
                        return false;
                    }
                    continue;
                }
                Some(Step::Await { call, state }) => {
                    self.awaiting = Some((call.clone(), state));
                    (call, Vec::new(), None)
                }
                Some(Step::Sleep(seconds)) => {
                    thread::sleep(Duration::from_secs(seconds));
                    continue;
                }
                Some(Step::Logged(kind)) => {
                    let Some(record) = &self.record else {
                        panic!("`await_log` reads the run's record, which `Sim::records` names")
                    };
                    logged(record, &format!(r#""kind":"{kind}""#));
                    continue;
                }
                Some(Step::Exit) => {
                    self.exit(0, replies);
                    continue;
                }
                Some(Step::Release) => {
                    self.released = true;
                    continue;
                }
                Some(Step::Result) => {
                    let score: i64 = self.read.trim().parse().unwrap();
                    self.result = Some(json!({ "raw_score": score }).to_string().into_bytes());
                    continue;
                }
                Some(Step::End(end)) => {
                    self.end = Some(end);
                    return true;
                }
                None if self.open => {
                    self.waits = Some(Wait::End);
                    return false;
                }
                None => {
                    self.exit(0, replies);
                    return false;
                }
            };
            self.make(call, payload, less, replies);
            if let Some(&Step::End(end)) = self.steps.front() {
                self.end = Some(end);
            }
            return self.end.is_some();
        }
    }

    fn make(
        &mut self,
        call: OpCall,
        payload: Vec<u8>,
        less: Option<u64>,
        replies: &mut Vec<(FromSandbox, Vec<u8>)>,
    ) {
        (self.op, self.less) = (self.op + 1, less);
        let (op, size) = (self.op, u64::try_from(payload.len()).unwrap());
        replies.push((FromSandbox::Op { op, call, size }, Vec::new()));
        if size > 0 {
            replies.push((FromSandbox::OpData { op }, payload));
        }
    }

    fn request(
        &mut self,
        (model, chat): (&str, bool),
        said: &[String],
        fields: Map<String, Value>,
        replies: &mut Vec<(FromSandbox, Vec<u8>)>,
    ) -> bool {
        if !self.endpoint {
            self.print("model request: connection refused");
            self.steps.clear();
            self.exit(1, replies);
            return false;
        }
        let request = self.model.map_or(0, |request| request + 1);
        (self.model, self.open) = (Some(request), true);
        self.read.clear();
        // Each request repeats the conversation's first item, as a harness's requests do.
        let item = |text: &str| json!({ "type": "message", "role": "user", "content": text });
        let mut input = vec![item("You are a simulated agent."), item(&said[0])];
        input.extend(said[1..].iter().map(|part| {
            let content = json!([{ "type": "input_text", "text": part }]);
            json!({ "type": "message", "role": "user", "content": content })
        }));
        let mut body = fields;
        body.insert("model".to_owned(), json!(model));
        let (conversation, path) = if chat {
            ("messages", "/v1/chat/completions")
        } else {
            body.insert("store".to_owned(), json!(false));
            ("input", "/responses")
        };
        body.entry(conversation).or_insert(json!(input));
        let body = Value::Object(body).to_string();
        let (method, path) = ("POST".to_owned(), path.to_owned());
        let header = |name: &str, value: &str| (name.to_owned(), value.to_owned());
        let headers = vec![
            header("content-type", "application/json"),
            header("user-agent", "sim"),
        ];
        let size = u64::try_from(body.len()).unwrap();
        let head = FromSandbox::ModelRequest {
            request,
            method,
            path,
            headers,
            size,
        };
        replies.push((head, Vec::new()));
        replies.push((FromSandbox::ModelData { request }, body.into_bytes()));
        true
    }

    fn heard(
        &mut self,
        response: ToSandbox,
        body: &[u8],
        replies: &mut Vec<(FromSandbox, Vec<u8>)>,
    ) -> Option<bool> {
        let (request, part) = match response {
            ToSandbox::ModelResponse {
                request,
                status,
                headers,
            } => {
                self.print(&format!("model request {request}: {status} {headers:?} "));
                (request, None)
            }
            ToSandbox::ModelData { request } => {
                let text = String::from_utf8_lossy(body);
                self.read.push_str(&text);
                self.stdout.push_str(&text.replace('\n', "\\n"));
                (request, Some(Wait::Part))
            }
            ToSandbox::ModelEnd { request, .. } => (request, Some(Wait::End)),
            other => panic!("{other:?} is no frame of a model response"),
        };
        if self.model != Some(request) {
            return None;
        }
        self.open &= part != Some(Wait::End);
        if part.is_none() || self.waits != part {
            return Some(false);
        }
        self.waits = None;
        Some(self.next(replies))
    }

    // Each line ends as the next begins, so that what it printed of a response is the same
    // whether the response's end comes before its output is collected or after.
    fn print(&mut self, text: &str) {
        if !self.stdout.is_empty() {
            self.stdout.push('\n');
        }
        self.stdout.push_str(text);
    }

    fn exit(&mut self, code: i32, replies: &mut Vec<(FromSandbox, Vec<u8>)>) {
        if self.exited {
            return;
        }
        let (stream, status) = (self.stream, ExitStatus::Code(code));
        replies.push((FromSandbox::Exited { stream, status }, Vec::new()));
        self.exited = true;
    }
}

fn reached(reply: &OpReply, state: &str) -> bool {
    let Some(end) = state.strip_prefix("transfer ") else {
        return state_name(reply) == state;
    };
    let OpReply::LeaseStatus(LeaseStatusReply {
        last: Some(Last::Transfer(TransferStatus {
            end: Some(ended), ..
        })),
        ..
    }) = reply
    else {
        return false;
    };
    json!(ended).as_object().unwrap().contains_key(end)
}

fn state_name(reply: &OpReply) -> String {
    match reply {
        OpReply::Status(StatusReply { state, .. }) => match state {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Completed { .. } => "completed",
            JobState::Errored { .. } => "errored",
        }
        .to_owned(),
        OpReply::LeaseStatus(LeaseStatusReply { state, .. }) => {
            json!(state).as_str().unwrap().to_owned()
        }
        _ => String::new(),
    }
}

// A step is an op, `ops`, which `ops at least <seconds> less` bounds, `status <job>` or `call
// <function> <payload> [<name>=<value>]...`, where a payload that is a number is that many bytes
// of `x`, and any other is its own bytes; `<session>.open [<seconds>]`, `<session>.run <lease>
// <seconds>|- <argv>...`, without a timeout for `-`, `<session>.status <lease>` or
// `<session>.close <lease>`; `<session>.put <lease> <tree> <to> [replace]` or `<session>.get
// <lease> <from> <to>`. An op is made once the one before it is answered. `ops at least <seconds>
// less` exits 1 unless the reply's remaining seconds fell by `seconds` since an earlier ops reply.
//
// `await <job> <state>` makes `status <job>` until its reply's state is `state`; `await_lease
// <session> <lease> <state>` likewise with `<session>.status <lease>`; `await_transfer <session>
// <lease> moved|failed` until its last transfer ended so. `await_log <kind>` waits until the run
// log holds an entry of kind `kind`, in the record that `Sim::records` named: what a run's other
// children log has an order against the agent's acts only through it.
//
// `model <model> <text> [<part>...]` is a model request that names `model`, with `text` as its
// input, then each `part`, and the next step waits for its response's end; the body holds too the
// fields of each part that is a JSON object, in place of an input, or of the conversation if they
// hold its field. `chat` is alike through Chat Completions; `ask <model> <text>` is one whose
// response's first part the next step waits for.
//
// `result` writes a result, `{"raw_score": …}`, of what a lease's command printed, or a model
// answered; `sleep <seconds>`; `exit` exits the command with 0, and the steps after it still play,
// as a process the command left would; `release` lets a collect of a directory named `held` go, and
// any later one. `crash` and `close` end the session: after an op, `crash` reads nothing more as
// the op goes out, so that its reply, which can't be written, ends the session, and `close` closes
// the connection, so that the end reaches the gateway behind the op.
fn step(arg: &str) -> Step {
    let words: Vec<&str> = arg.split(' ').collect();
    let grant = || Name::try_from(words[1].to_owned()).unwrap();
    let payload = || match words[2].parse() {
        Ok(size) => vec![b'x'; size],
        Err(_) => words[2].as_bytes().to_vec(),
    };
    let (call, payload, less) = match words[..] {
        ["ops"] => (OpCall::Ops, Vec::new(), None),
        ["ops", "at", "least", seconds, "less"] => {
            (OpCall::Ops, Vec::new(), Some(seconds.parse().unwrap()))
        }
        ["status", job] => {
            let job = job.parse().unwrap();
            (OpCall::Status { job }, Vec::new(), None)
        }
        ["call", _, _, ref given @ ..] => {
            let arguments = arguments(given);
            let call = OpCall::Call {
                grant: grant(),
                arguments,
            };
            (call, payload(), None)
        }
        ["await", job, state] => {
            let (call, state) = (
                OpCall::Status {
                    job: job.parse().unwrap(),
                },
                state.to_owned(),
            );
            return Step::Await { call, state };
        }
        [function, ..] if function.contains('.') => (session_call(&words), Vec::new(), None),
        ["await_lease", grant, lease, state] => {
            let call = lease_status(grant, lease);
            return Step::Await {
                call,
                state: (*state).to_owned(),
            };
        }
        ["await_transfer", grant, lease, end] => {
            let call = lease_status(grant, lease);
            let state = format!("transfer {end}");
            return Step::Await { call, state };
        }
        [kind @ ("model" | "chat" | "ask"), model, _, ..] => {
            return model_step(kind, model, &words[2..]);
        }
        ["sleep", seconds] => return Step::Sleep(seconds.parse().unwrap()),
        ["await_log", kind] => return Step::Logged((*kind).to_owned()),
        ["exit"] => return Step::Exit,
        ["release"] => return Step::Release,
        ["result"] => return Step::Result,
        ["crash"] => return Step::End(End::Crash),
        ["close"] => return Step::End(End::Close),
        _ => panic!("`{arg}` is no step of the simulated sandboxd's"),
    };
    Step::Op {
        call,
        payload,
        less,
    }
}

// The caller's arguments that `given` writes, each `<name>=<value>`.
fn arguments(given: &[&str]) -> BTreeMap<Name, String> {
    (given.iter())
        .map(|text| {
            let (name, value) = text.split_once('=').unwrap();
            (Name::try_from(name.to_owned()).unwrap(), value.to_owned())
        })
        .collect()
}

fn model_step(kind: &str, model: &str, words: &[&str]) -> Step {
    let (fields, said): (Vec<&str>, Vec<&str>) =
        (words.iter()).partition(|word| word.starts_with('{'));
    let fields = (fields.into_iter())
        .flat_map(|fields| serde_json::from_str::<Map<String, Value>>(fields).unwrap())
        .collect();
    let wait = if kind == "ask" { Wait::Part } else { Wait::End };
    Step::Model {
        model: model.to_owned(),
        chat: kind == "chat",
        said: said.into_iter().map(str::to_owned).collect(),
        fields,
        wait,
    }
}

/// Waits, for a minute at most, until the run log in `record` holds `part` of a line.
pub(crate) fn logged(record: &Path, part: &str) {
    for _ in 0..6000 {
        let log = fs::read_to_string(record.join("log.jsonl")).unwrap_or_default();
        if log.contains(part) {
            return;
        }
        thread::sleep(AWAIT_POLL);
    }
    panic!("the run log held no `{part}` in a minute");
}

fn lease_status(grant: &str, lease: &str) -> OpCall {
    OpCall::Session {
        grant: Name::try_from(grant.to_owned()).unwrap(),
        op: SessionOp::Status {
            lease: lease.parse().unwrap(),
        },
    }
}

fn session_call(words: &[&str]) -> OpCall {
    let (grant, op) = words[0].split_once('.').unwrap();
    let lease = || words[1].parse().unwrap();
    let op = match (op, &words[1..]) {
        ("open", seconds) => SessionOp::Open {
            instance_seconds: seconds.first().map(|seconds| seconds.parse().unwrap()),
        },
        ("run", [_, timeout, command @ ..]) => {
            let command = command
                .iter()
                .map(|&arg| arg.to_owned())
                .collect::<Vec<_>>();
            SessionOp::Run {
                lease: lease(),
                argv: Argv::try_from(command).unwrap(),
                timeout_seconds: timeout.parse().ok(),
            }
        }
        ("put", [_, tree, to, replace @ ..]) => SessionOp::Put {
            lease: lease(),
            tree: AbsolutePath::try_from((*tree).to_owned()).unwrap(),
            to: AbsolutePath::try_from((*to).to_owned()).unwrap(),
            replace: replace == ["replace"],
        },
        ("get", [_, from, to]) => SessionOp::Get {
            lease: lease(),
            from: AbsolutePath::try_from((*from).to_owned()).unwrap(),
            to: AbsolutePath::try_from((*to).to_owned()).unwrap(),
        },
        ("status", [_]) => SessionOp::Status { lease: lease() },
        ("close", [_]) => SessionOp::Close { lease: lease() },
        _ => panic!(
            "`{}` is no session op of the simulated sandboxd's",
            words.join(" ")
        ),
    };
    OpCall::Session {
        grant: Name::try_from(grant.to_owned()).unwrap(),
        op,
    }
}

#[derive(Default)]
struct Files {
    by_path: BTreeMap<String, Vec<u8>>,
    dirs: BTreeSet<String>,
    modes: BTreeMap<String, Mode>,
    dir: Option<AbsolutePath>,
    file: Option<String>,
}

impl Files {
    fn begin(
        &mut self,
        stream: u32,
        dir: AbsolutePath,
        root: Root,
        replies: &mut Vec<(FromSandbox, Vec<u8>)>,
    ) {
        let below = format!("{dir}/");
        let inside = |path: &String| *path == dir.as_str() || path.starts_with(&below);
        if root == Root::Replaced {
            self.dirs.retain(|path| !inside(path));
            self.by_path.retain(|path, _| !inside(path));
            self.modes.retain(|path, _| !inside(path));
        }
        let held = self.dirs.contains(dir.as_str())
            || (self.dirs.iter().chain(self.by_path.keys())).any(|path| path.starts_with(&below));
        let new = root != Root::Kept;
        if new && held {
            let message = format!("{dir}: File exists (os error 17)");
            replies.push((FromSandbox::Failed { stream, message }, Vec::new()));
            self.dir = None;
            return;
        }
        if new {
            self.dirs.insert(dir.to_string());
        }
        self.dir = Some(dir);
    }

    fn entry(&mut self, entry: Entry) {
        let Some(dir) = &self.dir else {
            return;
        };
        match entry {
            Entry::Dir { path, mode } => {
                let path = dir.join(&path).to_string();
                self.dirs.insert(path.clone());
                self.modes.insert(path, mode);
            }
            Entry::File { path, mode, .. } => {
                let path = dir.join(&path).to_string();
                self.modes.insert(path.clone(), mode);
                self.by_path.insert(path.clone(), Vec::new());
                self.file = Some(path);
            }
        }
    }

    fn data(&mut self, body: &[u8]) {
        if self.dir.is_none() {
            return;
        }
        let file = (self.file.as_ref()).and_then(|file| self.by_path.get_mut(file));
        file.unwrap().extend_from_slice(body);
    }

    fn end(&mut self, stream: u32, replies: &mut Vec<(FromSandbox, Vec<u8>)>) {
        if self.dir.take().is_some() {
            replies.push((FromSandbox::Done { stream }, Vec::new()));
        }
    }

    fn mode(&self, path: &str, made: Mode) -> Mode {
        self.modes.get(path).copied().unwrap_or(made)
    }

    fn below<'f>(&'f self, dir: &str) -> impl Iterator<Item = (&'f str, &'f [u8])> {
        let inside = format!("{dir}/");
        let files = self.by_path.iter();
        files.filter_map(move |(path, bytes)| Some((path.strip_prefix(&inside)?, &bytes[..])))
    }
}

fn judge(
    payload: Option<&Vec<u8>>,
    stream: u32,
    replies: &mut Vec<(FromSandbox, Vec<u8>)>,
) -> Option<Vec<u8>> {
    let exit = |code| {
        let status = ExitStatus::Code(code);
        (FromSandbox::Exited { stream, status }, Vec::new())
    };
    match payload.map(Vec::as_slice) {
        Some(b"slow") => None,
        Some(payload) => {
            replies.push(exit(0));
            let result = json!({ "raw_score": payload.len() });
            let mut result = result.to_string().into_bytes();
            if payload == b"large" {
                result.resize(usize::try_from(RESULT_MAX_BYTES).unwrap() + 1, b' ');
            }
            Some(result)
        }
        None => {
            replies.push(exit(1));
            None
        }
    }
}

/// Where the sim's `judge` writes its result, which the tests' bodies get.
const RESULT: &str = "/pumpkin/out/result.json";

fn collect(
    stream: u32,
    dir: &AbsolutePath,
    served: &mut Served,
    replies: &mut Vec<(FromSandbox, Vec<u8>)>,
) {
    // A collect of a directory named `held` waits for the `release` step.
    if dir.as_str().ends_with("/held") && !served.caller.as_ref().is_some_and(|c| c.released) {
        served.held = Some((stream, dir.clone()));
        return;
    }
    // A collect of a file sends the tree of that one file, by its name, as `sandboxd`'s does.
    let printed = served.printed.get(dir.as_str()).map(String::as_bytes);
    let caller = served.caller.as_ref();
    let written = caller.and_then(|caller| caller.result.as_deref());
    let result = (dir.as_str() == RESULT)
        .then(|| served.result.as_deref().or(written))
        .flatten();
    let put = served.files.by_path.get(dir.as_str()).map(Vec::as_slice);
    if let Some(body) = put.or(printed).or(result) {
        let (_, name) = dir.as_str().rsplit_once('/').unwrap();
        send_file(
            stream,
            name,
            served.files.mode(dir.as_str(), file_mode()),
            body,
            replies,
        );
        replies.push((FromSandbox::End { stream }, Vec::new()));
        return;
    }
    let inside = format!("{dir}/");
    for full in &served.files.dirs {
        if let Some(path) = full.strip_prefix(&inside) {
            let path = RelativePath::try_from(path.to_owned()).unwrap();
            let mode = served.files.mode(full, dir_mode());
            let entry = Entry::Dir { path, mode };
            replies.push((FromSandbox::Entry { stream, entry }, Vec::new()));
        }
    }
    let served = &*served;
    let put = served
        .files
        .below(dir.as_str())
        .any(|(path, _)| path == COLLECTED);
    let mut tree = Vec::new();
    if !put {
        tree.push((COLLECTED, COLLECTED.as_bytes()));
    }
    if *dir == pumpkin_dir().join(&out_dir()) {
        let caller = served.caller.as_ref();
        let written = caller.and_then(|caller| caller.result.as_deref());
        let result = served.result.as_deref().or(written);
        tree.extend(result.map(|result| ("result.json", result)));
    }
    let stdout = served.caller.as_ref().map_or("", |caller| &caller.stdout);
    if *dir == pumpkin_dir().join(&log_dir()) && !stdout.is_empty() {
        tree.push(("command.stdout", stdout.as_bytes()));
    }
    let printed = (served.printed.iter()).filter_map(|(path, text)| {
        let name = path.strip_prefix(dir.as_str())?.strip_prefix('/')?;
        (!name.contains('/')).then_some((name, text.as_bytes()))
    });
    tree.extend(printed);
    tree.extend(served.files.below(dir.as_str()));
    for (path, body) in tree {
        let mode = served.files.mode(&format!("{dir}/{path}"), file_mode());
        send_file(stream, path, mode, body, replies);
    }
    replies.push((FromSandbox::End { stream }, Vec::new()));
}

fn send_file(
    stream: u32,
    path: &str,
    mode: Mode,
    body: &[u8],
    replies: &mut Vec<(FromSandbox, Vec<u8>)>,
) {
    let path = RelativePath::try_from(path.to_owned()).unwrap();
    let size = u64::try_from(body.len()).unwrap();
    let entry = Entry::File { path, size, mode };
    replies.push((FromSandbox::Entry { stream, entry }, Vec::new()));
    for data in body.chunks(frame::MAX_BODY_BYTES) {
        replies.push((FromSandbox::Data { stream }, data.to_vec()));
    }
}

#[derive(Default)]
struct Served {
    caller: Option<Caller>,
    files: Files,
    result: Option<Vec<u8>>,
    printed: BTreeMap<String, String>,
    running: BTreeSet<u32>,
    held: Option<(u32, AbsolutePath)>,
}

impl Served {
    fn start(
        &mut self,
        stream: u32,
        launch: &Launch,
        model: bool,
        replies: &mut Vec<(FromSandbox, Vec<u8>)>,
        shared: &Mutex<Status>,
    ) -> bool {
        replies.push((FromSandbox::Done { stream }, Vec::new()));
        match launch.argv.program() {
            "exit" => {
                let code = launch.argv.args()[0].parse().unwrap();
                let status = ExitStatus::Code(code);
                replies.push((FromSandbox::Exited { stream, status }, Vec::new()));
            }
            "stall" => status(shared).stalled = true,
            "crash" => return true,
            "ops" => {
                let steps = launch.argv.args().iter().map(|arg| step(arg)).collect();
                let record = status(shared).record.clone();
                let made = (self.caller).insert(Caller::new(stream, steps, model, record));
                return made.next(replies);
            }
            "judge" => {
                let args = launch.argv.args();
                let payload = self.files.by_path.get(&args[0]);
                self.result = judge(payload, stream, replies);
                if let (Some(file), Some(result)) = (args.get(1), &self.result) {
                    self.files.by_path.insert(file.clone(), result.clone());
                }
            }
            "list" => {
                let dir = &launch.argv.args()[0];
                let inside = format!("{dir}/");
                // By path, each directory's ending in `/`.
                let mut lines = BTreeMap::new();
                for full in &self.files.dirs {
                    if let Some(path) = full.strip_prefix(&inside) {
                        let mode = self.files.mode(full, dir_mode());
                        lines.insert(format!("{path}/"), format!("{mode} {path}/"));
                    }
                }
                for (path, bytes) in self.files.below(dir) {
                    let mode = self.files.mode(&format!("{inside}{path}"), file_mode());
                    let text = String::from_utf8_lossy(bytes);
                    lines.insert(path.to_owned(), format!("{mode} {path}: {text}"));
                }
                let text = (lines.into_values()).collect::<Vec<_>>().join("\n");
                self.printed.insert(launch.stdout.to_string(), text);
                let status = ExitStatus::Code(0);
                replies.push((FromSandbox::Exited { stream, status }, Vec::new()));
            }
            "print" | "fill" => {
                let mut args = launch.argv.args();
                let mut text = String::new();
                if launch.argv.program() == "fill" {
                    text = "x".repeat(args[0].parse().unwrap());
                    args = &args[1..];
                }
                text.push_str(&args.join(" "));
                self.printed.insert(launch.stdout.to_string(), text);
                let status = ExitStatus::Code(0);
                replies.push((FromSandbox::Exited { stream, status }, Vec::new()));
            }
            _ => {
                self.running.insert(stream);
            }
        }
        false
    }

    fn kill(&mut self, stream: u32, command: u32, replies: &mut Vec<(FromSandbox, Vec<u8>)>) {
        if self.running.remove(&command) {
            let status = ExitStatus::Signal(9);
            let exited = FromSandbox::Exited {
                stream: command,
                status,
            };
            replies.push((exited, Vec::new()));
        }
        replies.push((FromSandbox::Done { stream }, Vec::new()));
    }
}

fn serve(mut connection: Connection, model: bool, shared: &Mutex<Status>) {
    let (mut replies, mut crashed) = (Vec::<(FromSandbox, Vec<u8>)>::new(), false);
    let mut served = Served::default();
    let (freezing, mut frozen) = (status(shared).freezing, false);
    // A connection that ends, cleanly or not, ends the session, as a delete's does.
    while let Ok(Some((request, body))) = frame::read::<ToSandbox>(&mut connection) {
        if status(shared).stalled {
            // It reads nothing more, as a stuck `sandboxd`, and answers nothing; the connection
            // stays open, by the clone its status keeps until a delete shuts it down, so the
            // session's writes fill the connection's buffers.
            return;
        }
        let Served {
            caller,
            files,
            result: _,
            printed: _,
            running: _,
            held: _,
        } = &mut served;
        match request {
            ToSandbox::Put { stream, dir, root } => files.begin(stream, dir, root, &mut replies),
            ToSandbox::Entry { entry, .. } => files.entry(entry),
            ToSandbox::Data { .. } => files.data(&body),
            ToSandbox::End { stream } => files.end(stream, &mut replies),
            ToSandbox::Freeze { stream } => {
                let (reply, froze) = freezing.freeze(stream);
                frozen |= froze;
                replies.push((reply, Vec::new()));
            }
            ToSandbox::Start { stream, launch } => {
                crashed = served.start(stream, &launch, model, &mut replies, shared);
            }
            ToSandbox::Kill { stream, command } => served.kill(stream, command, &mut replies),
            ToSandbox::Collect { stream, path } if freezing.changes(&path, frozen) => {
                let message =
                    format!("{path} changed while it was collected: freeze the sandbox first");
                replies.push((FromSandbox::Failed { stream, message }, Vec::new()));
            }
            ToSandbox::Collect { stream, path } => {
                collect(stream, &path, &mut served, &mut replies);
            }
            ToSandbox::OpReply { op, reply } => match caller {
                Some(caller) if caller.op == op => {
                    if let Some(journal) = &status(shared).journal {
                        let kind = json!(reply)["kind"].as_str().unwrap().to_owned();
                        // The session numbers its ops from 1 (`Caller::op`), the run log from 0.
                        journal.tell(format!("a session read op {}'s reply: {kind}", op - 1));
                    }
                    crashed = caller.answered(&reply, &mut replies);
                }
                // A reply to no op in flight breaks the protocol, which ends the session.
                Some(_) | None => return,
            },
            response @ (ToSandbox::ModelResponse { .. }
            | ToSandbox::ModelData { .. }
            | ToSandbox::ModelEnd { .. }) => {
                let heard = caller
                    .as_mut()
                    .map(|caller| caller.heard(response, &body, &mut replies));
                // An answer to no model request in flight breaks the protocol, which ends the
                // session.
                let Some(Some(next)) = heard else { return };
                crashed = next;
            }
        }
        if served.caller.as_ref().is_some_and(|caller| caller.released)
            && let Some((stream, dir)) = served.held.take()
        {
            collect(stream, &dir, &mut served, &mut replies);
        }
        let caller = served.caller.as_ref();
        let close = caller.and_then(|caller| caller.end) == Some(End::Close);
        if !send(&mut connection, &mut replies, crashed, close) {
            return;
        }
    }
}

fn send(
    connection: &mut Connection,
    replies: &mut Vec<(FromSandbox, Vec<u8>)>,
    crashed: bool,
    close: bool,
) -> bool {
    // An op that goes out as the session ends goes out after reading has stopped, so that its
    // reply, which follows its arrival, can't be written, and that failure alone ends the
    // session: were the connection closed too, its end's reason would be whichever of the close
    // and the failed write the gateway saw first, which its threads race to, and the reply could
    // be written before the close, so that no failure of it is noted.
    let op_out = crashed
        && !close
        && (replies.iter()).any(|(reply, _)| matches!(reply, FromSandbox::Op { .. }));
    if op_out {
        let _ = connection.shutdown(Shutdown::Read);
    }
    for (reply, body) in replies.drain(..) {
        if frame::write(connection, &reply, &body).is_err() {
            return false;
        }
    }
    if crashed && !op_out {
        // A shutdown closes the connection for the clone its status keeps too.
        let _ = connection.shutdown(Shutdown::Both);
    }
    !crashed
}
