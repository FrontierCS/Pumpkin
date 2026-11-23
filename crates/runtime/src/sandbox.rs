//! The one path every sandbox of a run starts on, and where each is recorded and deleted.
//!
//! [`Published`] holds what a run puts on its providers before the first start. [`Sandboxes`]
//! starts sandboxes from it and remembers each one it created, so that a delete is recorded
//! against the sandbox's own record, and [`sweep`] deletes whatever a provider still lists of
//! the run.

use std::cell::{Cell, RefCell};
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Mutex;
use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime};
use std::{fmt, io};

use decision::{JobId, LeaseId};
use gateway::{Arrival, Dialer, Listener, Session, Sockets};
use provider::{
    Admits, Create, Created, Creator, ImageRef, Listed, Meeting, Provider, Role, RunTag, SandboxId,
    SandboxdRef, Sweep, Undeleted, UnixTime,
};
use schema::ir::{Command, GpuModel, Image, Name};
use schema::sandbox::tree::Limits;
use schema::sandbox::{
    Endpoint, Launch, Listening, OpPayloadLimit, PUMPKIN_BYTES, PrivateDial, RelativePath,
    SocketDial, command_env, command_stderr, command_stdout, pumpkin_dir,
};
use schema::workflow::{
    AgentId, ByProvider, ByRole, JobFunctionId, Placed, RoleId, RunnerId, SessionId, TargetId,
    Workflow,
};
use serde::{Deserialize, Deserializer, de};
use stdx::{Context, Deadline};

use super::{Clock, Host, Input, Stopped, interruption, left, log, record, until};

/// The most entries, files and directories, that the host collects from a sandbox in one tree.
pub(super) const ENTRIES: u64 = 100_000;

// What a sandbox's `/pumpkin` holds at most: anything collected from below it fits.
const PUMPKIN: Limits = Limits {
    bytes: PUMPKIN_BYTES,
    entries: ENTRIES,
};

/// The simultaneous instances reserved in each phase. Each function owns one slot of its
/// target; each session owns a range of runner slots for every instance of its holder.
pub(super) struct Capacity {
    work: BTreeMap<RoleId, NonZeroU32>,
    final_phase: BTreeMap<RoleId, NonZeroU32>,
    pub(super) functions: BTreeMap<JobFunctionId, u32>,
    pub(super) sessions: BTreeMap<SessionId, u32>,
}

impl Capacity {
    pub(super) fn of(workflow: &Workflow) -> Result<Self, String> {
        let mut work = BTreeMap::new();
        let mut final_phase = BTreeMap::new();
        let final_role = workflow.final_call().target.role();
        final_phase.insert(final_role, NonZeroU32::MIN);
        for agent in workflow.agents() {
            work.insert(agent.id.role(), NonZeroU32::MIN);
        }
        for target in workflow.work_targets() {
            work.insert(target.role(), NonZeroU32::MIN);
        }
        let overflow = |role| {
            format!(
                "role `{}` needs more than {} simultaneous sandboxes: reduce its grants",
                workflow.sandbox(role).name,
                u32::MAX
            )
        };
        let reserve = |phase: &mut BTreeMap<_, NonZeroU32>, role, count: u32| {
            let first = phase.get(&role).map_or(0, |count| count.get());
            let total = first
                .checked_add(count)
                .and_then(NonZeroU32::new)
                .ok_or_else(|| overflow(role))?;
            phase.insert(role, total);
            Ok::<_, String>(first)
        };
        let mut functions = BTreeMap::new();
        for (id, function) in workflow.job_functions() {
            if workflow.target(function.target).lifetime == schema::ir::TargetLifetime::Call {
                functions.insert(id, reserve(&mut work, function.target.role(), 1)?);
            }
        }
        let mut sessions = BTreeMap::new();
        for (id, grant) in workflow.sessions() {
            let holder = grant.holder.role();
            let runner = grant.runner.role();
            let phase = if holder == final_role {
                &mut final_phase
            } else {
                &mut work
            };
            let count = phase[&holder]
                .get()
                .checked_mul(grant.instances.get())
                .ok_or_else(|| overflow(runner))?;
            sessions.insert(id, reserve(phase, runner, count)?);
        }
        Ok(Self {
            work,
            final_phase,
            functions,
            sessions,
        })
    }

    pub(super) fn phases(&self, placed: &Placed, provider: &Name) -> Vec<Vec<Role>> {
        [&self.work, &self.final_phase]
            .into_iter()
            .map(|phase| {
                phase
                    .iter()
                    .filter(|(role, _)| placed.placement(**role).provider == *provider)
                    .map(|(&role, &instances)| provider_role(placed.workflow(), role, instances))
                    .collect::<Vec<_>>()
            })
            .filter(|phase| !phase.is_empty())
            .collect()
    }

    fn instances(&self, role: RoleId) -> NonZeroU32 {
        self.work
            .get(&role)
            .into_iter()
            .chain(self.final_phase.get(&role))
            .copied()
            .max()
            .unwrap_or(NonZeroU32::MIN)
    }
}

fn provider_role(workflow: &Workflow, role: RoleId, instances: NonZeroU32) -> Role {
    let sandbox = workflow.sandbox(role);
    Role {
        name: sandbox.name.clone(),
        user: sandbox.user,
        resources: sandbox.needs.resources(),
        instances,
    }
}

/// What a run puts on each provider before any sandbox starts, and the directory of its sockets.
pub(super) struct Published<'p> {
    providers: ByProvider<Hosting<'p>>,
    images: ByRole<ImageRef>,
    sockets: Sockets,
}

/// Starts, records and deletes the sandboxes of one run.
pub(super) struct Sandboxes<'p, H: Host> {
    run: RunTag,
    pub(super) capacity: &'p Capacity,
    placed: &'p Placed,
    providers: ByProvider<Hosting<'p>>,
    images: ByRole<ImageRef>,
    sockets: Sockets,
    record: &'p Path,
    host: &'p H,
    logged: Sender<Input>,
    // Every sandbox created, which tells a delete where the record keeps it. Shared by the
    // threads that start and delete sandboxes: the loop's and each child's.
    inventory: Mutex<Inventory<'p>>,
    // How long a start waits between its lists of its sandbox's provider.
    between_looks: Duration,
}

#[derive(Default)]
struct Inventory<'p> {
    created: Vec<Made<'p>>,
    // A provider can fail a create after allocating an instance whose ID it cannot return.
    unnamed: BTreeSet<&'p Name>,
}

#[derive(Clone)]
struct Made<'p> {
    on: &'p Name,
    id: SandboxId,
    which: Which,
    deleted: bool,
    gpus: Option<Vec<GpuModel>>,
}

struct Hosting<'p> {
    provider: &'p dyn Provider,
    sandboxd: SandboxdRef,
    meets: Meets<'p>,
}

/// How a provider's sandboxes and the gateway meet ([`Meeting`]), with what the run supplies for
/// it.
#[derive(Clone, Copy)]
enum Meets<'p> {
    Sockets(&'p dyn Creator<SocketDial>),
    Private(
        &'p dyn Creator<PrivateDial, (Created, Admits)>,
        &'p Listener,
    ),
    Dials(&'p dyn Creator<Listening, (Created, Endpoint)>, &'p Dialer),
}

impl Meets<'_> {
    /// Issues what the new sandbox dials, creates it, and returns the registration of its session,
    /// which awaits it by `by`.
    fn create(
        self,
        sockets: &Sockets,
        create: &Create,
        by: Deadline,
    ) -> Result<(Created, gateway::Registration), String> {
        let issued = |error: io::Error| error.to_string();
        match self {
            Self::Sockets(creator) => {
                let (dial, registration) = sockets.issue(&create.name).map_err(issued)?;
                Ok((creator.create(create, dial)?, registration))
            }
            Self::Private(creator, listener) => {
                let (dial, unattested) = listener.issue(by);
                let (created, admits) = creator.create(create, dial)?;
                Ok((created, unattested.admitting(admits)))
            }
            Self::Dials(creator, dialer) => {
                let (dial, unreached) = dialer.issue().map_err(issued)?;
                let (created, endpoint) = creator.create(create, dial)?;
                Ok((created, unreached.at(endpoint)))
            }
        }
    }
}

/// Whose sandbox: the agent's, a call's target, or an instance of a lease's runner.
#[derive(Clone, Copy)]
pub(super) enum Which {
    Agent(AgentId),
    Shared(TargetId),
    Call(JobId, TargetId, u32),
    Lease(LeaseId, RunnerId, u32),
}

impl Which {
    fn role(self) -> RoleId {
        match self {
            Self::Agent(agent) => agent.role(),
            Self::Shared(target) | Self::Call(_, target, _) => target.role(),
            Self::Lease(_, runner, _) => runner.role(),
        }
    }

    fn instance(self) -> u32 {
        match self {
            Self::Agent(_) | Self::Shared(_) => 0,
            Self::Call(_, _, instance) | Self::Lease(_, _, instance) => instance,
        }
    }

    pub(super) fn record_dir(self, workflow: &Workflow) -> String {
        match self {
            Self::Agent(agent) => {
                format!("agents/{}", workflow.sandbox(agent.role()).name)
            }
            Self::Shared(target) => format!("shared/{}", workflow.sandbox(target.role()).name),
            Self::Call(job, ..) => format!("jobs/{}", job.0),
            Self::Lease(lease, ..) => format!("leases/{}", lease.0),
        }
    }

    pub(super) fn noun(self) -> &'static str {
        match self {
            Self::Agent(_) => "the agent's",
            Self::Shared(_) => "the shared role's",
            Self::Call(..) => "the job's",
            Self::Lease(..) => "the runner's",
        }
    }

    /// The sandbox's name, unique within its run.
    pub(super) fn name(self) -> Name {
        let name = match self {
            Self::Agent(agent) => SandboxName::Agent(agent.role().index()),
            Self::Shared(target) => SandboxName::Shared(target.role().index()),
            Self::Call(job, ..) => SandboxName::Job(job.0),
            Self::Lease(lease, ..) => SandboxName::Lease(lease.0),
        };
        // A `Name` made of such parts, which couldn't fail, would be a `pub` item of the schema
        // for this one caller.
        let Ok(name) = Name::try_from(name.to_string()) else {
            unreachable!(
                "a sandbox's name within its run is no name, though it is a prefix \
                 `agent-`, `shared-`, `job-`, or `lease-` and decimal digits, at most 26 characters of \
                 the 64 that a name allows"
            )
        };
        name
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// A sandbox's name within its run, as [`Which::name`] makes it.
pub(super) enum SandboxName {
    Agent(usize),
    Shared(usize),
    Job(u64),
    Lease(u64),
}

impl fmt::Display for SandboxName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent(index) => write!(f, "agent-{index}"),
            Self::Shared(index) => write!(f, "shared-{index}"),
            Self::Job(job) => write!(f, "job-{job}"),
            Self::Lease(lease) => write!(f, "lease-{lease}"),
        }
    }
}

impl<'de> Deserialize<'de> for SandboxName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        let numbered = |prefix: &str| name.strip_prefix(prefix)?.parse().ok();
        if let Some(index) = name.strip_prefix("agent-").and_then(|n| n.parse().ok()) {
            Ok(Self::Agent(index))
        } else if let Some(index) = name.strip_prefix("shared-").and_then(|n| n.parse().ok()) {
            Ok(Self::Shared(index))
        } else if let Some(job) = numbered("job-") {
            Ok(Self::Job(job))
        } else if let Some(lease) = numbered("lease-") {
            Ok(Self::Lease(lease))
        } else {
            Err(de::Error::invalid_value(
                de::Unexpected::Str(&name),
                &"`agent-<n>`, `shared-<n>`, `job-<n>` or `lease-<n>`",
            ))
        }
    }
}

pub(super) enum Unstarted {
    Uncreated(String),
    Created {
        which: Which,
        id: SandboxId,
        reason: String,
    },
}

impl From<String> for Unstarted {
    fn from(reason: String) -> Self {
        Self::Uncreated(reason)
    }
}

impl Unstarted {
    pub(super) fn reason(&self) -> &str {
        match self {
            Self::Uncreated(reason) | Self::Created { reason, .. } => reason,
        }
    }

    /// Returns the start failure with console evidence. An error also means that deletion
    /// or its record failed, so this instance cannot acknowledge the work teardown.
    pub(super) fn dispose(self, sandboxes: &Sandboxes<'_, impl Host>) -> Result<String, String> {
        match self {
            Self::Uncreated(reason) => Ok(reason),
            Self::Created { which, id, reason } => sandboxes.unstarted(which, &id, &reason),
        }
    }
}

/// A sandbox whose session is up, to be deleted through [`Sandboxes::delete`].
pub(super) struct Started {
    which: Which,
    id: SandboxId,
    pub(super) session: Session,
}

impl<'p> Published<'p> {
    /// Publishes `sandboxd` on each provider that hosts a role, and each role's image there, once
    /// however many roles name it, then makes the run's sockets directory in `record`, which
    /// must not exist yet. A provider that is private meets the gateway at `private`, which then
    /// must be some; one whose sandboxes listen, through `dialer`.
    ///
    /// Stops as interrupted before each publish once a signal has come, as a publish may take
    /// minutes.
    pub(super) fn create(
        providers: &ByProvider<&'p dyn Provider>,
        placed: &Placed,
        sandboxd: &Path,
        private: Option<&'p Listener>,
        dialer: &'p Dialer,
        record: &Path,
    ) -> Result<Self, Stopped> {
        let workflow = placed.workflow();
        let providers = placed.by_provider(|name| {
            // Each publish is a provider call that a stop doesn't cut short, and one may take
            // minutes, as a push does; so a signal is heeded before each, not after them all.
            interruption::check()?;
            let provider = *providers.get(name);
            let meets = match provider.meeting() {
                Meeting::Local(creator) => Meets::Sockets(creator),
                Meeting::Private(creator) => {
                    let listener = private.ok_or_else(|| {
                        format!(
                            "private provider `{name}` has no private listener: bind one for \
                             its sandboxes to dial"
                        )
                    })?;
                    Meets::Private(creator, listener)
                }
                Meeting::Listen(creator) => Meets::Dials(creator, dialer),
            };
            let published = provider.publish_sandboxd(sandboxd);
            let sandboxd = published.context(format_args!(
                "publishing `sandboxd` on provider `{name}` failed"
            ))?;
            Ok::<_, Stopped>(Hosting {
                provider,
                sandboxd,
                meets,
            })
        })?;
        let mut published = BTreeMap::<(&Name, Image), ImageRef>::new();
        let images = workflow.by_role(|(role, sandbox)| {
            let on = &placed.placement(role).provider;
            match published.entry((on, sandbox.image.clone())) {
                Entry::Occupied(entry) => Ok(entry.get().clone()),
                Entry::Vacant(entry) => {
                    interruption::check()?;
                    let image = providers.get(on).provider.publish_image(&entry.key().1);
                    let image = image.context(format_args!(
                        "publishing role `{}`'s image on provider `{on}` failed",
                        sandbox.name
                    ))?;
                    Ok::<_, Stopped>(entry.insert(image).clone())
                }
            }
        })?;
        // In the record, so that they go where it goes: `record::create` made it, and checked the
        // path to it, so that no other user may rename entries in it, or above it, as
        // `Sockets::create` needs.
        let sockets =
            Sockets::create(record::sockets(record)).map_err(|error| error.to_string())?;
        Ok(Self {
            providers,
            images,
            sockets,
        })
    }
}

impl<'p, H: Host> Sandboxes<'p, H> {
    pub(super) fn new(
        published: Published<'p>,
        run: RunTag,
        placed: &'p Placed,
        capacity: &'p Capacity,
        record: &'p Path,
        host: &'p H,
        logged: Sender<Input>,
    ) -> Self {
        let Published {
            providers,
            images,
            sockets,
        } = published;
        // A start ends within a fortieth of its role's startup, or 15 s, and a list's time, of its
        // sandbox's end; so the looks follow the run's quickest startup.
        let between_looks = (placed.workflow().roles())
            .fold(Duration::from_secs(15), |between, (role, _)| {
                between.min(placed.startup(role) / 40)
            });
        Self {
            run,
            capacity,
            placed,
            providers,
            images,
            sockets,
            record,
            host,
            logged,
            inventory: Mutex::default(),
            between_looks,
        }
    }

    /// Creates and registers a sandbox by `by`, its startup bound as a time on `clock`. On failure
    /// the caller owns disposal, so it can release prepared peers before a console read or
    /// deletion.
    pub(super) fn start<T: Send + 'static>(
        &self,
        which: Which,
        ops: Option<OpPayloadLimit>,
        (clock, by): (&H::Clock, Duration),
        hard_end: UnixTime,
        arrivals: Sender<T>,
        wrap: impl Fn(Arrival) -> T + Send + 'static,
    ) -> Result<Started, Unstarted> {
        let name = which.name();
        let now = UnixTime::of(SystemTime::now())?;
        // The start's deadline by the machine's clock, which the provider reads, at most the hard
        // end, as the run is abandoned then.
        let by_unix = left(clock, by).map_or(now, |left| now.saturating_add(left));
        let role = which.role();
        let model = self.workflow().model(role).is_some();
        let placement = self.placed.placement(role);
        let hosting = self.providers.get(&placement.provider);
        let create = Create {
            run: self.run.clone(),
            name,
            role: provider_role(self.workflow(), role, self.capacity.instances(role)),
            instance: which.instance(),
            class: placement.class.clone(),
            image: self.images.get(role).clone(),
            sandboxd: hosting.sandboxd.clone(),
            ops: ops.is_some(),
            model,
            hard_end,
            by: by_unix.min(hard_end),
        };
        // The private listener's wait for a check, which the machine's clock times.
        let by_machine = Deadline::after(left(clock, by).unwrap_or_default());
        let started = hosting.meets.create(&self.sockets, &create, by_machine);
        let (Created { id }, registration) = started.inspect_err(|_| {
            let mut inventory = stdx::unpoisoned(self.inventory.lock());
            inventory.unnamed.insert(&placement.provider);
        })?;
        let looks = Looks::new(clock);
        let made = Made {
            on: &placement.provider,
            id: id.clone(),
            which,
            deleted: false,
            gpus: None,
        };
        let recorded = self.write_sandbox(&made, None);
        // Listed before the write's failure is looked at, as `unstarted` finds it there.
        stdx::unpoisoned(self.inventory.lock()).created.push(made);
        if let Err(error) = recorded {
            return Err(Unstarted::Created {
                which,
                id,
                reason: error,
            });
        }
        // The wait ends at once for a signal, and for a sandbox that its provider lists as
        // ended, which will never connect.
        let stopped = || {
            interruption::signal().is_some()
                || looks.ended(self.between_looks, || self.ended(hosting.provider, &id))
        };
        let accepted = registration.accept(|| left(clock, by), ops, model, arrivals, wrap, stopped);
        let accepted = accepted.map_err(|error| match looks.found() {
            (true, _) => "it ended before it met the gateway: expected its `sandboxd` to run \
                          until it connects"
                .to_owned(),
            (false, Some(failed)) if error.kind() == io::ErrorKind::TimedOut => {
                format!("{error}; the last list of its provider failed: {failed}")
            }
            (false, _) => error.to_string(),
        });
        let set_up = |mut session: Session| session.set_up(until(clock, by)).map(|()| session);
        let session = accepted.and_then(set_up);
        let session = session.and_then(|session| match session.gpus() {
            Some(gpus) => self.saw(which, &id, gpus).map(|()| session),
            None => Ok(session),
        });
        match session {
            Ok(session) => Ok(Started { which, id, session }),
            Err(error) => Err(Unstarted::Created {
                which,
                id,
                reason: error,
            }),
        }
    }

    fn saw(&self, which: Which, id: &SandboxId, gpus: &[GpuModel]) -> Result<(), String> {
        let on = self.on(which);
        let mut inventory = stdx::unpoisoned(self.inventory.lock());
        let made = (inventory.created.iter_mut()).filter(|made| made.on == on && made.id == *id);
        let made: Vec<Made<'_>> = made
            .map(|made| {
                made.gpus = Some(gpus.to_vec());
                made.clone()
            })
            .collect();
        // Written once the lock is let go, so that no other thread waits on the record's syncs.
        drop(inventory);
        let saw = log::Entry::Gpus(which.name(), gpus.to_vec());
        // The loop's inbox outlives every thread that starts a sandbox.
        let _ = self.logged.send(Input::Logged(saw));
        let failures: Vec<String> = (made.iter())
            .filter_map(|made| self.write_sandbox(made, None).err())
            .collect();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }

    // A list leaves a sandbox out only once it is deleted, or has ended and been freed
    // (`Sweep::list`), so only a list that finds it running says it hasn't ended.
    fn ended(&self, provider: &dyn Provider, id: &SandboxId) -> Result<bool, String> {
        let listed = provider.list(&self.run)?;
        Ok(!(listed.iter()).any(|listed| listed.id == *id && !listed.ended))
    }

    // Deletes a sandbox that didn't start. The message is `error` with the sandbox's console and
    // the failure of the delete or of its record, if any.
    fn unstarted(&self, which: Which, id: &SandboxId, error: &str) -> Result<String, String> {
        let provider = self.provider(which);
        let mut parts = vec![format!("sandbox {}: {error}", id.as_str())];
        match provider.console(&self.run, id) {
            Ok(console) => {
                let text = console.text.trim();
                if !text.is_empty() {
                    parts.push(format!("its console ends: {text}"));
                }
            }
            Err(error) => parts.push(format!("its console can't be read: {error}")),
        }
        let deleted = provider.delete(&self.run, std::slice::from_ref(id));
        let (logged, recorded) =
            self.record_deletes(self.on(which), std::slice::from_ref(id), &deleted);
        self.send(logged);
        let disposed = deleted.is_ok() && recorded.is_ok();
        if let Err(error) = deleted {
            parts.push(format!("deleting it failed: {error}"));
        }
        parts.extend(recorded.err());
        let reason = parts.join("; ");
        if disposed { Ok(reason) } else { Err(reason) }
    }

    pub(super) fn workflow(&self) -> &'p Workflow {
        self.placed.workflow()
    }

    fn on(&self, which: Which) -> &'p Name {
        &self.placed.placement(which.role()).provider
    }

    fn provider(&self, which: Which) -> &'p dyn Provider {
        self.providers.get(self.on(which)).provider
    }

    /// Deletes the sandbox and returns its log entries and any failure of the delete or its record.
    pub(super) fn delete(&self, started: Started) -> (Vec<log::Entry>, Result<(), String>) {
        let Started { which, id, session } = started;
        let provider = self.provider(which);
        let deleted = provider.delete(&self.run, std::slice::from_ref(&id));
        drop(session);
        let (logged, recorded) =
            self.record_deletes(self.on(which), std::slice::from_ref(&id), &deleted);
        let deleted = deleted.context("deleting the sandbox failed");
        (logged, super::failed_also(deleted, recorded))
    }

    fn write_sandbox(
        &self,
        made: &Made<'_>,
        deleted: Option<Result<(), &str>>,
    ) -> Result<(), String> {
        let Made {
            on,
            id,
            which,
            gpus,
            ..
        } = made;
        let dir = self.record.join(which.record_dir(self.workflow()));
        record::write_sandbox(self.host, &dir, on, id, gpus.as_deref(), deleted)
    }

    // Writes how `deleted` went for each of `ids` into its record, if this run created it. Returns
    // the log entries of the delete, one per id it deleted, named by its sandbox if this run
    // created it, and the failure of the writes, if any.
    fn record_deletes(
        &self,
        on: &Name,
        ids: &[SandboxId],
        deleted: &Result<(), Undeleted>,
    ) -> (Vec<log::Entry>, Result<(), String>) {
        let undeleted = deleted.as_ref().err();
        let failed = |id: &SandboxId| undeleted.and_then(|undeleted| undeleted.why(id));
        let mut inventory = stdx::unpoisoned(self.inventory.lock());
        let made: Vec<_> = inventory
            .created
            .iter_mut()
            .filter(|made| made.on == on && ids.contains(&made.id))
            .map(|made| {
                made.deleted = failed(&made.id).is_none();
                made.clone()
            })
            .collect();
        // File syncs don't hold the lock shared by concurrent creates and deletes.
        drop(inventory);
        let mut logged = Vec::new();
        for id in ids.iter().filter(|id| failed(id).is_none()) {
            let made = made.iter().find(|made| made.id == *id);
            let deleted = Deleted {
                sandbox: made.map(|made| made.which.name()),
                provider: on.clone(),
                id: id.clone(),
            };
            logged.push(log::Entry::Deleted(deleted));
        }
        let failures: Vec<String> = (made.iter())
            .filter_map(|made| {
                let deleted = failed(&made.id).map_or(Ok(()), Err);
                self.write_sandbox(made, Some(deleted)).err()
            })
            .collect();
        let recorded = if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        };
        (logged, recorded)
    }

    fn send(&self, logged: Vec<log::Entry>) {
        for entry in logged {
            // The loop's inbox outlives every thread that deletes a sandbox.
            let _ = self.logged.send(Input::Logged(entry));
        }
    }

    /// After work drains, sweeps providers whose failed creates could have allocated an instance.
    /// A provider can list such an instance only later. It never received a work command.
    pub(super) fn finish_work(&self) -> Result<(), String> {
        let unnamed = std::mem::take(&mut stdx::unpoisoned(self.inventory.lock()).unnamed);
        if unnamed.is_empty() {
            return Ok(());
        }
        self.sweep_on(|name| unnamed.contains(name))
    }

    /// Deletes what each provider still lists of the run, so that a run ends with none of its
    /// sandboxes left, and records each delete.
    pub(super) fn sweep(&self) -> Result<(), String> {
        self.sweep_on(|_| true)
    }

    fn sweep_on(&self, include: impl Fn(&Name) -> bool) -> Result<(), String> {
        let providers =
            self.providers
                .iter()
                .filter(|(name, _)| include(name))
                .map(|(name, hosting)| {
                    let provider: &dyn Sweep = hosting.provider;
                    (name, provider)
                });
        let record = |name: &Name, ids: &[SandboxId], deleted: &Result<(), Undeleted>| {
            let (logged, recorded) = self.record_deletes(name, ids, deleted);
            self.send(logged);
            recorded
        };
        let remaining: Vec<_> = stdx::unpoisoned(self.inventory.lock())
            .created
            .iter()
            .filter(|made| !made.deleted)
            .map(|made| (made.on.clone(), made.id.clone()))
            .collect();
        let (_, failed) = sweep(providers, &self.run, &remaining, record);
        failed.map_or(Ok(()), Err)
    }
}

pub(super) struct Deleted {
    pub(super) sandbox: Option<Name>,
    pub(super) provider: Name,
    pub(super) id: SandboxId,
}

/// Deletes the sandboxes of `run` that each of `providers` lists, and those of `also` on it that
/// it doesn't, then calls `after` with the provider's name, the ids deleted and the result of the
/// delete, to record it, whichever it was. Nothing is deleted on a provider, nor `after` called,
/// if there are none.
///
/// Returns the ids that each provider whose sweep succeeded listed, by its name, once every
/// provider is swept, and why each list, delete or `after` that failed did, if one did.
pub(super) fn sweep<'p>(
    providers: impl IntoIterator<Item = (&'p Name, &'p dyn Sweep)>,
    run: &RunTag,
    also: &[(Name, SandboxId)],
    after: impl Fn(&Name, &[SandboxId], &Result<(), Undeleted>) -> Result<(), String>,
) -> (Vec<(Name, Vec<SandboxId>)>, Option<String>) {
    let (mut swept, mut failures) = (Vec::new(), Vec::new());
    for (name, provider) in providers {
        // A sandbox that its provider lists no more may have ended and left what only its delete
        // removes.
        let held = also.iter().filter(|(on, _)| on == name);
        let held: Vec<SandboxId> = held.map(|(_, id)| id.clone()).collect();
        let recorded = |ids: &[SandboxId], deleted: &_| after(name, ids, deleted);
        match sweep_one(name, provider, run, &held, recorded) {
            Ok(ids) => swept.push((name.clone(), ids)),
            Err(error) => failures.push(error),
        }
    }
    let failed = (!failures.is_empty()).then(|| failures.join("; "));
    (swept, failed)
}

fn sweep_one(
    name: &Name,
    provider: &dyn Sweep,
    run: &RunTag,
    also: &[SandboxId],
    after: impl FnOnce(&[SandboxId], &Result<(), Undeleted>) -> Result<(), String>,
) -> Result<Vec<SandboxId>, String> {
    let listed = provider.list(run).context(format_args!(
        "listing the run's sandboxes on provider `{name}` failed"
    ))?;
    let ids: Vec<SandboxId> = (listed.into_iter())
        .map(|Listed { id, ended: _ }| id)
        .collect();
    let unlisted = also.iter().filter(|id| !ids.contains(id)).cloned();
    let deleting: Vec<SandboxId> = ids.iter().cloned().chain(unlisted).collect();
    if !deleting.is_empty() {
        let deleted = provider.delete(run, &deleting);
        let recorded = after(&deleting, &deleted);
        let deleted = deleted.context(format_args!(
            "deleting the sandboxes the run left on provider `{name}` failed"
        ));
        super::failed_also(deleted, recorded)?;
    }
    Ok(ids)
}

/// Looks at whether a sandbox has ended, at most once per `between` by the run's clock, and keeps
/// the failure of the last look if no look has succeeded since.
struct Looks<'c, C> {
    clock: &'c C,
    last: Cell<Duration>,
    ended: Cell<bool>,
    failed: RefCell<Option<String>>,
}

impl<'c, C: Clock> Looks<'c, C> {
    fn new(clock: &'c C) -> Self {
        Self {
            clock,
            last: Cell::new(clock.now()),
            ended: Cell::new(false),
            failed: RefCell::new(None),
        }
    }

    // The first look comes `between` after the create returned, so every list speaks for the
    // sandbox it made.
    fn ended(&self, between: Duration, look: impl FnOnce() -> Result<bool, String>) -> bool {
        let since = self.clock.now().saturating_sub(self.last.get());
        if !self.ended.get() && since >= between {
            match look() {
                Ok(ended) => {
                    self.failed.replace(None);
                    self.ended.set(ended);
                }
                Err(error) => {
                    self.failed.replace(Some(error));
                }
            }
            self.last.set(self.clock.now());
        }
        self.ended.get()
    }

    fn found(self) -> (bool, Option<String>) {
        (self.ended.get(), self.failed.into_inner())
    }
}

/// How a sandbox runs `command`: with no standard input, and its output written into the
/// sandbox's `.pumpkin` directory.
pub(super) fn launch(command: &Command) -> Launch {
    let Command { argv, env, cwd } = command;
    Launch {
        argv: argv.clone(),
        env: command_env(env),
        cwd: cwd.clone(),
        stdin: None,
        stdout: pumpkin_dir().join(&command_stdout()),
        stderr: pumpkin_dir().join(&command_stderr()),
    }
}

/// Collects `dir` of the sandbox's `.pumpkin` directory into the same path under `record`, a new
/// directory there, within the `PUMPKIN` limits, by `by`. After a failure it holds what arrived
/// before.
pub(super) fn collect(
    session: &mut Session,
    dir: &RelativePath,
    record: &Path,
    by: &dyn Fn() -> Option<Duration>,
) -> Result<(), String> {
    collect_as(session, dir, &record.join(dir.as_str()), by)
}

pub(super) fn collect_as(
    session: &mut Session,
    dir: &RelativePath,
    into: &Path,
    by: &dyn Fn() -> Option<Duration>,
) -> Result<(), String> {
    let from = pumpkin_dir().join(dir);
    session
        .collect(&from, into, PUMPKIN, by)
        .context(format_args!("collecting {from} failed"))
}
