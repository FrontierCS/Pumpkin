//! A simulated provider, [`Sim`], which stands in for a platform, and passes the suite every
//! provider passes ([`suite`](test_support::suite)).
//!
//! Each sandbox is a thread of this process, running a fake `sandboxd` that dials its create's
//! dial, a socket's, at once; says hello and answers the session's requests,
//! running each command by its program, which may make ops or judge a payload ([`Sim`]). What it
//! does that a run acts on may go to a [`Journal`], in order with what other parts of a test's
//! simulated world tell it.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{self, Read, Write};
use std::net::Shutdown;
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
    JobState, Last, LeaseStatusReply, OpCall, OpReply, StatusReply, TransferStatus,
};
use schema::sandbox::{
    Entry, ExitStatus, FromSandbox, HelloReply, Launch, PROTOCOL_VERSION, RESULT_MAX_BYTES,
    RelativePath, Root, SOCKET_NAME, SocketDial, ToSandbox, dir_mode, file_mode, frame, log_dir,
    out_dir, pumpkin_dir,
};
use schema::workflow::{Class, Offers};
use serde_json::{Value, json};
use stdx::Deadline;
use test_support::suite::{Locals, Subject};

/// The path of a `sandboxd`, or an [`image`]'s name, that never dials: a create that names
/// it, as published, makes a sandbox whose `sandboxd` prints so, and ends.
pub(crate) const MUTE: &str = "/sim/mute";

/// The path of a `sandboxd`, or an [`image`]'s name, that never dials, as [`MUTE`]'s, but
/// hangs, as one stuck as it starts, rather than end, until its sandbox is deleted.
pub(crate) const HUNG: &str = "/sim/hung";

/// The path of a `sandboxd`, or an [`image`]'s name, that never dials, as [`MUTE`]'s.
///
/// The provider frees its sandbox once its `sandboxd` has ended, so the run's list leaves it out,
/// as [`Sweep::list`] lets such a provider.
pub(crate) const VANISHING: &str = "/sim/vanishing";

/// The path of a `sandboxd`, or an [`image`]'s name, that hangs without dialing, as
/// [`HUNG`]'s, and whose sandbox fails the run's list until it is deleted, as a provider's API
/// may fail.
pub(crate) const UNLISTABLE: &str = "/sim/unlistable";

/// The path of a `sandboxd` whose create fails, leaving behind a sandbox, as [`MUTE`]'s, that
/// the run's list still finds.
pub(crate) const LOST: &str = "/sim/lost";

/// The path of a `sandboxd`, or an [`image`]'s name, that dials, but says hello in the sandbox
/// protocol's next version, as a `sandboxd` built from a later Pumpkin would.
pub(crate) const LATER: &str = "/sim/later";

/// An [`image`]'s name whose create returns `SLOW_CREATE` after it starts the sandbox,
/// whose `sandboxd` has dialed by then.
///
/// A create whose deadline comes first gives up then, and fails, leaving the sandbox for the run's
/// list to find ([`Creator::create`](provider::Creator::create)), which, where it listens, ends itself as no gateway claims
/// it.
pub(crate) const SLOW: &str = "/sim/slow";

/// How long a create of [`SLOW`] takes: far longer than a run takes to act on what its agent
/// does as the create begins, or a `sandboxd` takes to dial, and far shorter than the startup of
/// the providers whose tests use it.
const SLOW_CREATE: Duration = Duration::from_secs(2);

/// The name of the simulated provider's second class, whose sandboxes take at most
/// `BRIEF_STARTUP` seconds to start, and the CPU model it states, so that it is offered only to
/// roles that need that model.
pub(crate) const BRIEF: &str = "brief";

/// How long a start in class `brief` takes at most: seconds, but still far longer than a
/// simulated sandbox takes to start, so that a busy machine makes a start that succeeds wait
/// longer, never fail.
const BRIEF_STARTUP: u64 = 2;

/// How long a command that awaits a job's state waits between the `status` ops it makes, and a
/// sandbox that listens between its looks for the gateway's connection, or its deletion.
const AWAIT_POLL: Duration = Duration::from_millis(10);

/// What every tree the simulated `sandboxd` collects holds: a file of this name, which holds the
/// same.
const COLLECTED: &str = "collected";

/// The path of a `sandboxd`, or an [`image`]'s name, that the simulated provider refuses to
/// publish.
pub(crate) const UNPUBLISHED: &str = "/sim/unpublished";

/// The path of a `sandboxd`, or an [`image`]'s name, that dials, but whose sandbox the
/// simulated provider refuses to delete.
///
/// Once a delete is refused, its console shows nothing, as an EC2 instance's once its terminate
/// was sent and the wait for it gave up.
pub(crate) const UNDELETABLE: &str = "/sim/undeletable";

/// The path of a `sandboxd`, or an [`image`]'s name, whose first delete fails, as one of
/// [`UNDELETABLE`]'s does.
///
/// The next deletes it, as a provider's API may fail a call once.
pub(crate) const REFUSED_ONCE: &str = "/sim/refused-once";

/// The path of a `sandboxd`, or an [`image`]'s name, that dials, but whose sandbox's console
/// the simulated provider fails to read.
pub(crate) const UNSHOWN: &str = "/sim/unshown";

/// The path of a `sandboxd`, or an [`image`]'s name, whose sandbox's delete waits `LINGER`
/// before it deletes it.
pub(crate) const LINGERING: &str = "/sim/lingering";

/// How long a delete of a [`LINGERING`] sandbox takes: seconds longer than the hard end of the
/// runs whose tests use it.
const LINGER: Duration = Duration::from_secs(7);

/// An [`image`]'s name whose sandbox's delete waits until each other sandbox of its run is
/// deleted, and no other delete is under way, as a platform may finish a delete that began later
/// first, and only then deletes it.
pub(crate) const LAST: &str = "/sim/last";

/// The longest a delete of a [`LAST`] sandbox waits: one that waits this long waits on a sandbox
/// that nothing deletes, the test's bug.
const LAST_WAITS: Duration = Duration::from_secs(60);

/// How long a sandbox's fake `sandboxd` takes to end once its sandbox is deleted.
// A platform's sandbox ends some time after it is told to, so a delete that doesn't wait for its
// end returns while it runs, which the suite then sees (`Subject::held`): far longer than the
// suite takes to look after a delete returns, and far shorter than any test's bounds.
const ENDING: Duration = Duration::from_millis(100);

/// A provider whose sandboxes are threads, each running a fake `sandboxd`.
///
/// A create whose deadline has passed as it begins fails, making nothing. A sandbox's `sandboxd`
/// says this version's hello, unless its create names a fault, such as [`MUTE`] or [`LATER`], and
/// stops reading once [stalled](Subject::stall); the sandbox is listed as ended once it exits
/// ([`VANISHING`] and [`UNLISTABLE`] aside). It keeps the files put, collects a tree of one file,
/// `COLLECTED`, from any directory, with what its commands wrote there (`collect`), and runs each
/// command by its program (`Served::start`). Each fault, such as [`UNPUBLISHED`], says in its doc
/// what it does.
pub(crate) struct Sim {
    /// The name of its platform, which each reference it publishes begins with, then `:`.
    platform: &'static str,
    ready: Ready,
    // `Provider`'s methods take `&self`, and it is `Sync`, so what they change is behind a lock,
    // which each instance that `Subject::connect` makes shares, as a platform's instances share
    // its sandboxes.
    state: Arc<Mutex<State>>,
    journal: Option<Journal>,
}

/// What the parts of a test's simulated world saw, in the order they told it, each from the
/// thread it saw it on, as it saw it.
#[derive(Clone, Default)]
pub(crate) struct Journal {
    // Shared, as each part tells it from its own thread, and the test reads it.
    entries: Arc<Mutex<Vec<String>>>,
}

impl Journal {
    /// Adds `entry` after what it holds.
    pub(crate) fn tell(&self, entry: String) {
        stdx::unpoisoned(self.entries.lock()).push(entry);
    }

    /// What it holds, in the order it was told.
    pub(crate) fn entries(&self) -> Vec<String> {
        stdx::unpoisoned(self.entries.lock()).clone()
    }
}

struct State {
    /// The number in the next sandbox's id.
    next: u64,
    /// The sandboxes made and not yet deleted, by id.
    sandboxes: BTreeMap<String, Sandbox>,
    /// The sandboxes deleted, which the platform holds until their threads have ended.
    deleted: Vec<Deleted>,
    /// How many deletes are under way.
    deleting: usize,
    /// The permission bits of the directory that held each create's socket directory, in the
    /// order of the creates that dialed a socket.
    modes: Vec<u32>,
    /// Whether each create asked for the model endpoint, in the order of the creates.
    models: Vec<bool>,
    /// The class of each create, in the order of the creates.
    classes: Vec<Name>,
    /// What each publish that succeeded published, `sandboxd` or an image, in the order of the
    /// publishes.
    published: Vec<String>,
    /// The CPUs it shares out between runs' reservations.
    cpus: u64,
    /// Each reservation made, live until its holder drops it.
    reserved: Vec<Reserved>,
}

/// A run's reservation of CPUs, which counts while its holder keeps it.
struct Reserved {
    run: RunTag,
    /// The roles it holds CPUs for, each with how many of its sandboxes run at once.
    roles: Vec<(Name, NonZeroU32)>,
    cpus: u64,
    live: Weak<()>,
}

struct Sandbox {
    run: RunTag,
    /// How many deletes it refuses yet, `u32::MAX` for every one.
    refusals: u32,
    /// How long its delete waits.
    linger: Duration,
    /// [`LAST`]'s.
    last: bool,
    unshown: bool,
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

    /// Whether its fake `sandboxd` has exited: its thread has returned, but not for a stall,
    /// which stands for a `sandboxd` that is stuck, as a paused container's is.
    fn ended(&self) -> bool {
        self.thread.is_finished() && !status(&self.shared).stalled
    }
}

/// What [`Subject::held`] keeps of a deleted sandbox.
struct Deleted {
    id: String,
    run: RunTag,
    /// Its status, which its thread keeps until it ends.
    shared: Weak<Mutex<Status>>,
}

/// How the run's list shows a sandbox.
enum Listing {
    /// As it is, ended or not.
    Shown,
    /// Left out once it has ended, as [`VANISHING`]'s.
    LeftOutOnceEnded,
    /// Not at all: the list fails while it lasts, as [`UNLISTABLE`]'s.
    Failing,
}

/// What a sandbox's thread and the [`Sim`] share.
#[derive(Default)]
struct Status {
    console: String,
    /// The thread's connection once dialed, by which a delete ends the session.
    connection: Option<Connection>,
    stalled: bool,
    deleted: bool,
    /// Where its session's reads go, if the provider has a journal.
    journal: Option<Journal>,
}

impl Status {
    /// The status of a sandbox as it starts, whose session's reads go to `journal`, if any.
    fn new(journal: Option<Journal>) -> Self {
        Self {
            journal,
            ..Self::default()
        }
    }
}

impl Sim {
    /// A simulated provider whose startup is 30 s.
    pub(crate) fn new() -> Self {
        // Far longer than a thread takes to start and dial, so that a busy machine makes a start
        // wait longer, never fail.
        Self::with_startup(30)
    }

    /// A simulated provider, of platform `sim`, that declares two classes, which offer far more
    /// CPU and memory than any role needs: `sim`, whose startup is `seconds`, and [`BRIEF`].
    ///
    /// # Panics
    ///
    /// Panics if `seconds` is 0.
    pub(crate) fn with_startup(seconds: u64) -> Self {
        Self {
            platform: "sim",
            ready: ready(NonZeroU64::new(seconds).unwrap()),
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
            })),
            journal: None,
        }
    }

    /// This provider, which tells `journal` each create once it has made its sandbox, as
    /// `created <id>`, each sandbox a delete deletes, as `deleted <id>`, and each reply to an op
    /// that a sandbox's session reads, as it reads it, as `a session read op <n>'s reply: <kind>`,
    /// where `n` numbers the session's ops from 0, as the run log does.
    #[must_use]
    pub(crate) fn journaled(mut self, journal: &Journal) -> Self {
        self.journal = Some(journal.clone());
        self
    }

    /// The journal it tells, if it has one.
    pub(crate) fn journal(&self) -> Option<&Journal> {
        self.journal.as_ref()
    }

    /// Fails unless `create`'s run holds CPUs for its role's instance.
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

    /// This provider, of the platform named `platform` in place of `sim`: each reference it
    /// publishes names its platform, and its create refuses one that names another, as a
    /// platform's references mean nothing to another's.
    #[must_use]
    pub(crate) fn named(self, platform: &'static str) -> Self {
        Self { platform, ..self }
    }

    /// This provider, which shares out `cpus` CPUs between the reservations of runs, each
    /// instance of a role counting its CPU limit rounded up to whole CPUs, and refuses a
    /// reservation that doesn't fit beside the others held.
    #[must_use]
    pub(crate) fn with_cpus(self, cpus: u64) -> Self {
        self.state().cpus = cpus;
        self
    }

    /// This provider, whose classes offer `memory_bytes` of memory each.
    ///
    /// # Panics
    ///
    /// Panics if `memory_bytes` is 0.
    #[must_use]
    pub(crate) fn offering_memory(mut self, memory_bytes: u64) -> Self {
        for class in self.ready.classes.values_mut() {
            class.offers.memory_bytes = NonZeroU64::new(memory_bytes).unwrap();
        }
        self
    }

    /// The permission bits of the directory that held each create's socket directory, in the
    /// order of the creates that dialed a socket.
    pub(crate) fn modes(&self) -> Vec<u32> {
        self.state().modes.clone()
    }

    /// Whether each create asked for the model endpoint, in the order of the creates.
    pub(crate) fn models(&self) -> Vec<bool> {
        self.state().models.clone()
    }

    /// The class of each create, in the order of the creates.
    pub(crate) fn classes(&self) -> Vec<Name> {
        self.state().classes.clone()
    }

    /// What each publish that succeeded published, in the order of the publishes: `sandboxd`, as
    /// `sandboxd <path>`, or an image, as `image <digest>`.
    pub(crate) fn published(&self) -> Vec<String> {
        self.state().published.clone()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        stdx::unpoisoned(self.state.lock())
    }

    /// What `reference`, the create's `what`, names on the platform: the rest of it, past the
    /// platform's name; or why it names nothing there.
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

/// What a simulated provider declares whose class `sim` has a startup of `startup_seconds`:
/// that class and [`BRIEF`], which states its model.
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
                    status(&sandbox.shared).console.clear();
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
        Meeting::Local(self)
    }

    fn console(&self, run: &RunTag, id: &SandboxId) -> Result<Console, String> {
        self.console_of(run, id)
    }
}

impl Creator<SocketDial> for Sim {
    fn create(&self, create: &Create, dial: SocketDial) -> Result<Created, String> {
        let gateway = Gateway::Socket(dial.dir.join(SOCKET_NAME));
        self.made(create, gateway)
    }
}

impl Sim {
    /// Creates the sandbox that `create` describes, which meets the gateway at `gateway`.
    // A sandbox, a thread of this process, ends with it, so the hard end needs nothing more;
    // and the name needs no use, as each sandbox is created once.
    fn made(&self, create: &Create, gateway: Gateway) -> Result<Created, String> {
        let by = Deadline::at_unix(create.by.since_epoch());
        if by.remaining().is_none() {
            let passed =
                "the simulated create's deadline had passed: expected time left to make it";
            return Err(passed.to_owned());
        }
        self.holds(create)?;
        let path = self.own("the `sandboxd`", create.sandboxd.as_str())?;
        let digest = self.own("the image", create.image.as_str())?;
        let Faults {
            made,
            dialing,
            listing,
            refusals,
            linger,
            last,
            unshown,
        } = Faults::of(|fault: &str| path == fault || digest == image(fault).as_str());
        let (mute, hung) = dialing.silent();
        let hello = if dialing == Dialing::Later {
            json!({ "version": PROTOCOL_VERSION + 1 })
        } else {
            json!({ "version": PROTOCOL_VERSION })
        };
        let mode = gateway.mode();
        let shared = Arc::new(Mutex::new(Status::new(self.journal.clone())));
        // Before its thread starts, so that the console shows it once the create returns, however
        // late the thread runs.
        (status(&shared).console).push_str(if mute { NEVER_DIALS } else { "" });
        let (ours, model) = (Arc::clone(&shared), create.model);
        let thread = thread::Builder::new()
            .name("sim-sandboxd".to_owned())
            .spawn(move || sandboxd(&gateway, mute, hung, &hello, model, &ours))
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
            unshown,
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
            report: Value::Null,
        })
    }

    /// What sandbox `id` of `run` printed, as [`Provider::console`] asks.
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
        if sandbox.unshown {
            return Err(format!(
                "the simulated console of {} can't be read: create one with another `sandboxd`",
                id.as_str()
            ));
        }
        let text = status(&sandbox.shared).console.clone();
        Ok(Console { text })
    }
}

/// Where a simulated sandbox meets the gateway.
enum Gateway {
    /// At the socket of this path, of its own.
    Socket(PathBuf),
}

impl Gateway {
    /// The permission bits of the directory that holds a socket's directory.
    fn mode(&self) -> Option<u32> {
        match self {
            Self::Socket(socket) => {
                let parent = fs::metadata(socket.parent()?.parent()?).unwrap();
                Some(parent.permissions().mode() & 0o777)
            }
        }
    }
}

/// A simulated sandbox's connection to the gateway.
enum Connection {
    Unix(UnixStream),
}

impl Connection {
    /// Dials `gateway`.
    fn dial(gateway: &Gateway) -> io::Result<Self> {
        match gateway {
            Gateway::Socket(socket) => UnixStream::connect(socket).map(Self::Unix),
        }
    }

    fn try_clone(&self) -> io::Result<Self> {
        match self {
            Self::Unix(stream) => stream.try_clone().map(Self::Unix),
        }
    }

    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.shutdown(how),
        }
    }
}

impl Read for Connection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.read(buf),
        }
    }
}

impl Write for Connection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.flush(),
        }
    }
}

/// The image named `name`, such as a fault's, whose digest is `name`'s bytes, then zeros.
///
/// # Panics
///
/// Panics if `name` is over 32 bytes.
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
        // An ended sandbox that its list leaves out stands for one the platform has freed,
        // though the provider keeps it until it is deleted, to end its thread.
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

/// The faults a create simulates, each of the paths and image names above that it names.
struct Faults {
    made: Made,
    dialing: Dialing,
    listing: Listing,
    /// How many deletes it refuses, `u32::MAX` for every one, as [`UNDELETABLE`]'s, or one, as
    /// [`REFUSED_ONCE`]'s.
    refusals: u32,
    /// How long its delete waits: [`LINGERING`]'s.
    linger: Duration,
    /// [`LAST`]'s.
    last: bool,
    /// [`UNSHOWN`]'s.
    unshown: bool,
}

/// How a create ends.
#[derive(PartialEq, Eq)]
enum Made {
    /// It returns the sandbox.
    Named,
    /// It fails, leaving the sandbox, as [`LOST`]'s.
    Lost,
    /// It returns after [`SLOW_CREATE`], as [`SLOW`]'s.
    Slow,
}

/// What a sandbox's `sandboxd` does.
#[derive(PartialEq, Eq)]
enum Dialing {
    /// It dials and says hello.
    Hello,
    /// It dials and says hello in the next version, as [`LATER`]'s.
    Later,
    /// It never dials, and ends.
    Mute,
    /// It never dials, and hangs, as [`HUNG`]'s and [`UNLISTABLE`]'s.
    Hung,
}

impl Dialing {
    /// Whether the `sandboxd` never dials, and whether it hangs.
    fn silent(&self) -> (bool, bool) {
        match self {
            Self::Hello | Self::Later => (false, false),
            Self::Mute => (true, false),
            Self::Hung => (true, true),
        }
    }
}

impl Faults {
    /// The faults of a create that `names` each fault it simulates.
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
            linger: if names(LINGERING) {
                LINGER
            } else {
                Duration::ZERO
            },
            last: names(LAST),
            unshown: names(UNSHOWN),
        }
    }
}

/// Tells a deleted sandbox's thread to end, closing its connection if it has one, and waits
/// until it has, after its delete's wait.
fn stop(sandbox: Sandbox) {
    let Sandbox {
        run: _,
        refusals: _,
        linger,
        last: _,
        unshown: _,
        listing: _,
        shared,
        thread,
    } = sandbox;
    thread::sleep(linger);
    let mut status = status(&shared);
    status.deleted = true;
    if let Some(connection) = status.connection.take() {
        // A connection the gateway closed already is fine.
        let _ = connection.shutdown(Shutdown::Both);
    }
    drop(status);
    // A thread that hangs waits to be woken once its sandbox is deleted.
    thread.thread().unpark();
    // Only a bug panics the thread, and joining raises its panic here.
    thread.join().unwrap();
}

/// What the console of a sandbox whose `sandboxd` never dials shows.
const NEVER_DIALS: &str = "sandboxd: a simulated one, which never dials\n";

/// A sandbox's fake `sandboxd`, whose console its create has written: if `mute`, it waits until
/// the sandbox is deleted if `hung`, and else ends; or else it [dials](dial), saying `hello`.
/// Once its sandbox is deleted, it takes
/// [`ENDING`] to end.
fn sandboxd(
    gateway: &Gateway,
    mute: bool,
    hung: bool,
    hello: &Value,
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

/// Dials `gateway`, opens the session with `hello`, and serves it until the session ends, the
/// sandbox is deleted, or it is stalled, with the model endpoint if `model`.
fn dial(gateway: &Gateway, hello: &Value, model: bool, shared: &Mutex<Status>) {
    let dialed = Connection::dial(gateway).and_then(|connection| {
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
            if open(&mut connection, hello) {
                serve(connection, model, shared);
            }
        }
        Err(error) => {
            let at = match gateway {
                Gateway::Socket(socket) => socket.display().to_string(),
            };
            let failed = format!("sandboxd: connecting to the gateway at {at} failed: {error}");
            status.console.push_str(&failed);
        }
    }
}

/// Opens the session on `connection` with `hello`: whether the gateway accepted it.
fn open(connection: &mut Connection, hello: &Value) -> bool {
    let sent = frame::write(connection, hello, &[]);
    let answer = sent.and_then(|()| frame::read::<HelloReply>(connection));
    // Unlike `sandboxd`, it prints no refusal: a run reads the console of a start that failed as
    // soon as the refusal is sent, so what it found would depend on a race. A connection may also
    // end before the session opens, as a delete ends it.
    matches!(answer, Ok(Some((HelloReply::Accepted, _))))
}

/// A step of a command that makes ops.
enum Step {
    /// An op, with its payload, and, if bounded, the fewest seconds by which its reply must
    /// leave less time than the last `ops` reply did.
    Op {
        call: OpCall,
        payload: Vec<u8>,
        less: Option<u64>,
    },
    /// `call`, a `status` or a `lease_status`, made until its reply gives `state`.
    Await { call: OpCall, state: String },
    /// A model request that names `model`, whose input is `text`, and what of its response the
    /// next step waits for.
    Model {
        model: String,
        text: String,
        wait: Wait,
    },
    /// A wait, in seconds, before the next step.
    Sleep(u64),
    /// The command's exit with 0, after which the sandbox takes the steps that follow, as a
    /// process the command left would.
    Exit,
    /// The answer to the collect of a `held` directory, which waits for it ([`collect`]), and to
    /// any such collect later.
    Release,
    /// A judge's result, `{"raw_score": n}`, where `n` is what the last command of a lease printed,
    /// as the last `lease_status` reply gives it.
    Result,
    /// The session's end, which its `sandboxd` makes as [`End`] says.
    End(End),
}

/// How a fake `sandboxd` ends its session.
#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    /// Right after an op, by reading nothing more as the op goes out, so that the op's reply
    /// ends it, as it can't be written; otherwise by closing the connection.
    Crash,
    /// By closing the connection, right after an op too, so that the end reaches the gateway
    /// behind the op, whatever its owner does: a reply then races the gateway's reader to the
    /// close, unless the run's loop takes the op only once the end has reached it.
    Close,
}

/// What of a model response a command waits for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wait {
    /// The first part of its body.
    Part,
    End,
}

/// A command that makes ops.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is a fact of the fake command's own, which its steps set apart"
)]
struct Caller {
    /// The command's stream, on which it exits.
    stream: u32,
    /// The steps still to take, in order.
    steps: VecDeque<Step>,
    /// The number of the op in flight.
    op: u32,
    /// The bound on the reply to the op in flight, as [`Step::Op`] gives it.
    less: Option<u64>,
    /// The seconds the last `ops` reply left, once one has come.
    left: Option<u64>,
    /// The `status` or `lease_status` the op in flight asks, and the state it awaits, if it awaits
    /// one.
    awaiting: Option<(OpCall, String)>,
    /// Whether the command has exited.
    exited: bool,
    /// Whether its sandbox serves the model endpoint.
    endpoint: bool,
    /// The number of the last model request, once one has been made: the one in flight, as it
    /// makes one at a time.
    model: Option<u32>,
    /// Whether the response to the last model request has yet to end.
    open: bool,
    /// What of that response the next step waits for, if it waits for it.
    waits: Option<Wait>,
    /// What it has printed: each response to its model requests.
    stdout: String,
    /// What the last command of a lease printed, as the last `lease_status` reply gives it.
    read: String,
    /// The result it wrote, if it wrote one.
    result: Option<Vec<u8>>,
    /// How its `sandboxd` ends the session, once a step has asked it to.
    end: Option<End>,
    /// Whether a step has released the collects of `held` directories, the one waiting and any
    /// later.
    released: bool,
}

impl Caller {
    /// A command on `stream` that takes `steps`, in a sandbox that serves the model endpoint if
    /// `endpoint`, before it has taken any.
    fn new(stream: u32, steps: VecDeque<Step>, endpoint: bool) -> Self {
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
        }
    }

    /// Takes the reply to the op in flight, `reply`: if it is an `ops` that breaks the op's
    /// bound, or that follows no `ops` reply, the command exits with 1 at once, taking no more
    /// steps; if it gives another state than the one awaited, it asks again after
    /// [`AWAIT_POLL`]; else it goes on ([`Caller::next`]).
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

    /// Takes the steps up to the next op, and makes it, or exits with 0 once there are none,
    /// unless it has exited, in `replies`. Returns whether the session is to end: once the op
    /// before an end is sent, before its answer.
    fn next(&mut self, replies: &mut Vec<(FromSandbox, Vec<u8>)>) -> bool {
        loop {
            let (call, payload, less) = match self.steps.pop_front() {
                Some(Step::Op {
                    call,
                    payload,
                    less,
                }) => (call, payload, less),
                Some(Step::Model { model, text, wait }) => {
                    if self.request(&model, &text, replies) {
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

    /// Makes an op of `call`, with `payload`, whose reply is bounded by `less`, as [`Step::Op`]
    /// gives it, in `replies`.
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

    /// Makes a model request that names `model`, with `text` as its input, in `replies`, and
    /// returns whether it did: without the model endpoint, the command prints that the
    /// connection was refused, and exits with 1, taking no more steps.
    fn request(
        &mut self,
        model: &str,
        text: &str,
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
        // Each request repeats the conversation's first item, as a harness's requests do.
        let item = |text: &str| json!({ "type": "message", "role": "user", "content": text });
        let input = [item("You are a simulated agent."), item(text)];
        let body = json!({ "model": model, "store": false, "input": input }).to_string();
        let (method, path) = ("POST".to_owned(), "/responses".to_owned());
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

    /// Takes `response`, a frame of the response to a model request, with its `body`: prints
    /// its head on a line of its own, and its body after it, each line break shown as `\n`, and
    /// once what the next step waits for of it has come, takes the steps that follow
    /// ([`Caller::next`]), returning what that does. Returns `None` for a frame that answers no
    /// model request in flight.
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
                let text = String::from_utf8_lossy(body).replace('\n', "\\n");
                self.stdout.push_str(&text);
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

    /// Prints `text` on a line of its own.
    // Each line ends as the next begins, so that what it printed of a response is the same
    // whether the response's end comes before its output is collected or after.
    fn print(&mut self, text: &str) {
        if !self.stdout.is_empty() {
            self.stdout.push('\n');
        }
        self.stdout.push_str(text);
    }

    /// Exits with `code`, in `replies`, unless it has exited.
    fn exit(&mut self, code: i32, replies: &mut Vec<(FromSandbox, Vec<u8>)>) {
        if self.exited {
            return;
        }
        let (stream, status) = (self.stream, ExitStatus::Code(code));
        replies.push((FromSandbox::Exited { stream, status }, Vec::new()));
        self.exited = true;
    }
}

/// Whether `reply` gives `state`, a state's name, or `transfer <end>` for a lease's status whose
/// last is a transfer that ended so, `moved` or `failed`.
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

/// The state a status or a lease's status gives, as `sandboxctl` prints it, or `""` for any
/// other reply.
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

/// The step that `arg` gives, in the form the comment below lays out.
fn step(arg: &str) -> Step {
    // A step is an op, `ops`, which `ops at least <seconds> less` bounds, `status <job>`, `judge
    // <grant> <payload>`, `submit <grant> <payload>` or `finalize <grant> <payload>`, a final
    // submission, where a payload that is a number is that many bytes of `x`, and any other is its
    // own bytes; `lease_open <grant> [<seconds>]`, `lease_run <lease> <seconds>|- <argv>...`,
    // without a timeout for `-`, `lease_status <lease>` or `lease_close <lease>`; `await <job>
    // <state>`, which makes `status <job>` until its reply's state is `state`, or `await_lease
    // <lease> <state>`, `lease_status <lease>` likewise, or `await_transfer <lease> moved|failed`,
    // until its last transfer ended so; `lease_put <lease> <tree> <to> [replace]` or `lease_get
    // <lease> <from> <to>`; `model <model> <text>`, a model request that names `model`, with `text`
    // as its input, whose response's end the next step waits for, or `ask <model> <text>`, one
    // whose response's first part it waits for; `result`, a judge's result of what a lease's
    // command printed ([`Step::Result`]); or `sleep <seconds>`, `exit`, `release`
    // ([`Step::Release`]), or the session's end, `crash` or `close`.
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
        ["judge", _, _] => (OpCall::Judge { grant: grant() }, payload(), None),
        [kind @ ("submit" | "finalize"), _, _] => {
            let finalize = kind == "finalize";
            let grant = grant();
            (OpCall::Submit { grant, finalize }, payload(), None)
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
        [
            "lease_open" | "lease_run" | "lease_put" | "lease_get" | "lease_status" | "lease_close",
            ..,
        ] => (lease_call(&words), Vec::new(), None),
        ["await_lease", lease, state] => {
            let lease = lease.parse().unwrap();
            let (call, state) = (OpCall::LeaseStatus { lease }, state.to_owned());
            return Step::Await { call, state };
        }
        ["await_transfer", lease, end] => {
            let lease = lease.parse().unwrap();
            let (call, state) = (OpCall::LeaseStatus { lease }, format!("transfer {end}"));
            return Step::Await { call, state };
        }
        [kind @ ("model" | "ask"), model, text] => {
            let (model, text) = (model.to_owned(), text.to_owned());
            let wait = if kind == "model" {
                Wait::End
            } else {
                Wait::Part
            };
            return Step::Model { model, text, wait };
        }
        ["sleep", seconds] => return Step::Sleep(seconds.parse().unwrap()),
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

/// The lease op that `words`, a step's, make ([`step`]).
fn lease_call(words: &[&str]) -> OpCall {
    let lease = || words[1].parse().unwrap();
    match words {
        ["lease_open", grant, seconds @ ..] => OpCall::LeaseOpen {
            grant: Name::try_from((*grant).to_owned()).unwrap(),
            instance_seconds: seconds.first().map(|seconds| seconds.parse().unwrap()),
        },
        ["lease_run", _, timeout, command @ ..] => {
            let command = command
                .iter()
                .map(|&arg| arg.to_owned())
                .collect::<Vec<_>>();
            OpCall::LeaseRun {
                lease: lease(),
                argv: Argv::try_from(command).unwrap(),
                timeout_seconds: timeout.parse().ok(),
            }
        }
        ["lease_put", _, tree, to, replace @ ..] => OpCall::LeasePut {
            lease: lease(),
            tree: AbsolutePath::try_from((*tree).to_owned()).unwrap(),
            to: AbsolutePath::try_from((*to).to_owned()).unwrap(),
            replace: replace == ["replace"],
        },
        ["lease_get", _, from, to] => OpCall::LeaseGet {
            lease: lease(),
            from: AbsolutePath::try_from((*from).to_owned()).unwrap(),
            to: AbsolutePath::try_from((*to).to_owned()).unwrap(),
        },
        ["lease_status", _] => OpCall::LeaseStatus { lease: lease() },
        ["lease_close", _] => OpCall::LeaseClose { lease: lease() },
        _ => panic!(
            "`{}` is no lease op of the simulated sandboxd's",
            words.join(" ")
        ),
    }
}

/// The directories and files put in a sandbox, by path.
#[derive(Default)]
struct Files {
    by_path: BTreeMap<String, Vec<u8>>,
    dirs: BTreeSet<String>,
    /// The mode of each directory and file put, by path; one made only as a tree's root has
    /// [`dir_mode`]'s.
    modes: BTreeMap<String, Mode>,
    /// The directory of the tree being put, if one is, unless putting it failed.
    dir: Option<AbsolutePath>,
    /// The path of the file being put, if one is.
    file: Option<String>,
}

impl Files {
    /// Begins the tree put in `dir` on `stream`, made as `root` says, which fails, in `replies`,
    /// if it is made new in a directory that holds what was put before, as `sandboxd` fails it,
    /// and which first removes what was put there before to replace it.
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

    /// Ends the tree put on `stream`, answering it in `replies` unless it failed.
    fn end(&mut self, stream: u32, replies: &mut Vec<(FromSandbox, Vec<u8>)>) {
        if self.dir.take().is_some() {
            replies.push((FromSandbox::Done { stream }, Vec::new()));
        }
    }

    /// The mode of the directory or file at `path`, or, unless one was put there, `made`.
    fn mode(&self, path: &str, made: Mode) -> Mode {
        self.modes.get(path).copied().unwrap_or(made)
    }

    /// The files put below `dir`, by their paths below it.
    fn below<'f>(&'f self, dir: &str) -> impl Iterator<Item = (&'f str, &'f [u8])> {
        let inside = format!("{dir}/");
        let files = self.by_path.iter();
        files.filter_map(move |(path, bytes)| Some((path.strip_prefix(&inside)?, &bytes[..])))
    }
}

/// What a `judge` command, whose exit comes on `stream`, does with `payload`, the file put at
/// the path its argument gives, if there is one: for `slow`, it runs until the sandbox is
/// frozen; for `stall`, it stalls the sandbox; for any other, it writes the result
/// `{"raw_score": n}`, where `n` is the payload's size, which it returns,
/// and exits with 0, the result padded with spaces to a byte over [`RESULT_MAX_BYTES`] for
/// `large`. Without a payload, it exits with 1.
fn judge(
    payload: Option<&Vec<u8>>,
    stream: u32,
    replies: &mut Vec<(FromSandbox, Vec<u8>)>,
    shared: &Mutex<Status>,
) -> Option<Vec<u8>> {
    let exit = |code| {
        let status = ExitStatus::Code(code);
        (FromSandbox::Exited { stream, status }, Vec::new())
    };
    match payload.map(Vec::as_slice) {
        Some(b"slow") => None,
        Some(b"stall") => {
            status(shared).stalled = true;
            None
        }
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

/// Collects `dir` on `stream`, in `replies`: [`COLLECTED`], unless a file put there is named so,
/// from `out` the judge's `result`
/// too, if it wrote one, and from `log` what the command that makes ops printed, if anything;
/// each file in `dir` that a `print` wrote ([`Served::start`]); and each directory and file put
/// below `dir`. The collect of a directory named `held` waits for a step to release it
/// ([`Step::Release`]).
fn collect(
    stream: u32,
    dir: &AbsolutePath,
    served: &mut Served,
    replies: &mut Vec<(FromSandbox, Vec<u8>)>,
) {
    if dir.as_str().ends_with("/held") && !served.caller.as_ref().is_some_and(|c| c.released) {
        served.held = Some((stream, dir.clone()));
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
    // A tree put there, as one collected elsewhere first, may hold its own.
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
        let path = RelativePath::try_from(path.to_owned()).unwrap();
        let size = u64::try_from(body.len()).unwrap();
        let entry = Entry::File { path, size, mode };
        replies.push((FromSandbox::Entry { stream, entry }, Vec::new()));
        for data in body.chunks(frame::MAX_BODY_BYTES) {
            replies.push((FromSandbox::Data { stream }, data.to_vec()));
        }
    }
    replies.push((FromSandbox::End { stream }, Vec::new()));
}

/// What a fake `sandboxd`'s session holds of the commands it started: the one that makes ops, if
/// one has started, the files put, a judge's result, if one wrote it, what each `print` wrote, by
/// the path of its standard output, and the commands that run until the sandbox is frozen, or a
/// kill ends them, by stream.
#[derive(Default)]
struct Served {
    caller: Option<Caller>,
    files: Files,
    result: Option<Vec<u8>>,
    printed: BTreeMap<String, String>,
    running: BTreeSet<u32>,
    /// The collect of a `held` directory, by its stream, until a step releases it.
    held: Option<(u32, AbsolutePath)>,
}

impl Served {
    /// Starts `launch` on `stream`, in a sandbox that serves the model endpoint if `model`,
    /// answering in `replies`, and returns whether its `sandboxd` is to end the session, as its
    /// program says (the comment below).
    fn start(
        &mut self,
        stream: u32,
        launch: &Launch,
        model: bool,
        replies: &mut Vec<(FromSandbox, Vec<u8>)>,
        shared: &Mutex<Status>,
    ) -> bool {
        // By its program: `exit` exits at once with the status its argument gives; `stall` starts,
        // then stalls the sandbox; `crash` starts, then its `sandboxd` ends the session; `ops`
        // takes the steps its arguments give (`step`), making each op, or model request, once the
        // one before it is answered, then exits with 0, once the response to its last model request
        // has ended, unless it has, or its `sandboxd` ends the session as a step asks, and prints
        // each response to its model requests, which it makes only with the model endpoint, and
        // whose end a freeze waits for; `judge` judges the payload put at the path its argument
        // gives (`judge`); `print` writes its arguments to its standard output, `fill <n> <text>`
        // that many `x`s and then the text, and `list` each directory and file put below the
        // directory its argument gives, by path, with its mode, and each file's contents, and each
        // exits with 0; any other runs until the sandbox is frozen, or a
        // kill ends it, as signal 9 does.
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
                let made = self.caller.insert(Caller::new(stream, steps, model));
                return made.next(replies);
            }
            "judge" => {
                let payload = self.files.by_path.get(&launch.argv.args()[0]);
                self.result = judge(payload, stream, replies, shared);
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

    /// Kills the command on stream `command`, if it runs, as signal 9 does, and answers the kill
    /// on `stream`, in `replies`.
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

/// Answers the session's requests on `connection` until the session ends, or, once stalled,
/// reads nothing more, serving the model endpoint if `model`.
fn serve(mut connection: Connection, model: bool, shared: &Mutex<Status>) {
    let (mut replies, mut crashed) = (Vec::<(FromSandbox, Vec<u8>)>::new(), false);
    let mut served = Served::default();
    // The stream of a freeze that waits for a model response to end.
    let mut freezing = None;
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
            // A freeze waits for the response to the model request in flight, if any, to end, so
            // that a run that cuts it short before the agent's teardown, as it must, sees its
            // freeze done, and one that would cut it after waits out its freeze's deadline.
            ToSandbox::Freeze { stream } => freezing = Some(stream),
            ToSandbox::Start { stream, launch } => {
                crashed = served.start(stream, &launch, model, &mut replies, shared);
            }
            ToSandbox::Kill { stream, command } => served.kill(stream, command, &mut replies),
            ToSandbox::Collect { stream, dir } => collect(stream, &dir, &mut served, &mut replies),
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
        if let Some(stream) = freezing
            && !caller.is_some_and(|caller| caller.open)
        {
            replies.push((FromSandbox::Done { stream }, Vec::new()));
            freezing = None;
        }
        let close = caller.and_then(|caller| caller.end) == Some(End::Close);
        if !send(&mut connection, &mut replies, crashed, close) {
            return;
        }
    }
}

/// Sends `replies` on `connection`, in order, then ends the session if `crashed`, as
/// [`End::Close`] says if `close`, and else as [`End::Crash`] does. Returns whether the session
/// goes on.
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
