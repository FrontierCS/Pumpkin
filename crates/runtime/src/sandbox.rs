//! The runtime's one start path, which every sandbox of a run starts on, on the provider its role
//! is placed on ([`Sandboxes::start`]), which ends a start once its sandbox has ended, as the run
//! ends every start once a signal has come, and what each role's sandbox needs of it: its IR
//! command launched, and what it wrote collected. Each sandbox that a start created is recorded
//! with what its provider made of it, the GPUs its `sandboxd` saw, and each delete's outcome
//! ([`record::write_sandbox`]); each delete keeps its console in the run's record first
//! ([`record::write_console`]).
//!
//! The provider's side of each step is [`Provider`]'s, and the session's [`Session`]'s.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Mutex;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant, SystemTime};
use std::{io, iter};

use decision::{Event, JobId, LeaseId};
use gateway::{Arrival, GrantListener, Session, Sockets};
use provider::{
    Console, Create, Created, Creator, ImageRef, Listed, Meeting, Provider, Role, RunTag,
    SandboxId, SandboxdRef, Sweep, UnixTime,
};
use schema::ir::{Command, GpuModel, Image, Name};
use schema::sandbox::tree::Limits;
use schema::sandbox::{
    Launch, OpPayloadLimit, PUMPKIN_BYTES, RelativePath, SocketDial, TcpDial, command_env,
    command_stderr, command_stdout, pumpkin_dir,
};
use schema::workflow::{ByProvider, ByRole, JudgeId, Placed, RoleId, RunnerId, Workflow};
use serde_json::Value;
use stdx::{Context, Deadline};

use super::{Host, Input, Stopped, interruption, log, record};

/// The most entries a collected tree may hold.
pub(super) const ENTRIES: u64 = 100_000;

/// What a sandbox's `/pumpkin` holds at most: anything collected from below it fits.
const PUMPKIN: Limits = Limits {
    bytes: PUMPKIN_BYTES,
    entries: ENTRIES,
};

/// The roles of `placed` that are placed on `provider`, as [`Provider::reserve`] takes them:
/// in the work phase, the agent, the judge of each of its judge grants, each grant running one
/// call at a time, and the runner of each lease grant those hold, as many as the grant's leases
/// open at once; then the score's judge, with the runners it leases, which start only once they
/// have gone.
pub(super) fn phases(placed: &Placed, provider: &Name) -> Vec<Vec<Role>> {
    let workflow = placed.workflow();
    let scorer = workflow.score().slot.judge().role();
    let judges = (workflow.judge_grants()).map(|(_, grant)| grant.judge().role());
    let runners = |last: bool| {
        (workflow.lease_grants())
            .filter(move |(_, grant)| (grant.holder == scorer) == last)
            .map(|(_, grant)| grant.runner.role())
    };
    let agent = iter::once(workflow.agent().role);
    let work: Vec<RoleId> = agent.chain(judges).chain(runners(false)).collect();
    let last: Vec<RoleId> = iter::once(scorer).chain(runners(true)).collect();
    let on = |roles: Vec<RoleId>| -> Vec<Role> {
        (roles.into_iter())
            .filter(|&role| placed.placement(role).provider == *provider)
            .map(|role| provider_role(workflow, role))
            .collect()
    };
    [on(work), on(last)]
        .into_iter()
        .filter(|phase| !phase.is_empty())
        .collect()
}

/// `role` of `workflow`, as a provider sees it: as many of its sandboxes run at once as the lease
/// grant that names it opens leases at once, if it is a runner, and else one.
fn provider_role(workflow: &Workflow, role: RoleId) -> Role {
    let sandbox = workflow.sandbox(role);
    let leased = (workflow.lease_grants()).find(|(_, grant)| grant.runner.role() == role);
    Role {
        name: sandbox.name.clone(),
        user: sandbox.user,
        resources: sandbox.needs.resources(),
        instances: leased.map_or(NonZeroU32::MIN, |(_, grant)| grant.instances),
    }
}

/// What the sandboxes of a run run, published on the providers its roles are placed on, and where
/// they meet the gateway: what the run makes of them before it starts ([`Published::create`]).
pub(super) struct Published<'p> {
    providers: ByProvider<Hosting<'p>>,
    /// Each role's image, as its provider published it.
    images: ByRole<ImageRef>,
    sockets: Sockets,
}

/// The sandboxes of one run of a workflow on the providers its roles are placed on, once the run
/// has started: what they run, published, and where they meet the gateway.
pub(super) struct Sandboxes<'p, H: Host> {
    run: RunTag,
    placed: &'p Placed,
    providers: ByProvider<Hosting<'p>>,
    /// Each role's image, as its provider published it.
    images: ByRole<ImageRef>,
    sockets: Sockets,
    /// The run's record, which keeps each sandbox that a start created, and its console.
    record: &'p Path,
    /// The run's host, which syncs each sandbox and console kept.
    host: &'p H,
    /// A sender into the loop's inbox, which takes the run log's record of the GPUs that each
    /// sandbox's `sandboxd` saw, and of each delete.
    logged: Sender<Input>,
    /// Each sandbox a start created, which tells the sweep where the record keeps it. Shared by
    /// the threads that start and delete sandboxes: the loop's and each judge call's.
    created: Mutex<Vec<Made<'p>>>,
    /// How long a start waits between its lists of its sandbox's provider: a fortieth of the
    /// quickest startup of the run's roles, at most 15 s.
    between_looks: Duration,
}

/// A sandbox that a start of the run created.
#[derive(Clone)]
struct Made<'p> {
    on: &'p Name,
    id: SandboxId,
    which: Which,
    /// What the provider reported it made beside the sandbox ([`Created::report`]).
    report: Value,
    /// The GPUs that its `sandboxd` saw as it started, once its session says them
    /// ([`Session::gpus`]).
    gpus: Option<Vec<GpuModel>>,
    /// Whether a read has kept its console, which the sweep then keeps no more.
    kept: bool,
}

/// A provider that roles of the run are placed on, the `sandboxd` it published, which each of
/// its creates names, and where its sandboxes meet the gateway.
struct Hosting<'p> {
    provider: &'p dyn Provider,
    sandboxd: SandboxdRef,
    meets: Meets<'p>,
}

/// Where a provider's sandboxes meet the gateway, as its [`Meeting`] says, with its create.
#[derive(Clone, Copy)]
enum Meets<'p> {
    /// At a socket of each one's own, among the run's [`Sockets`].
    Sockets(&'p dyn Creator<SocketDial>),
    /// At the address of the run's grant listener.
    Grants(&'p dyn Creator<TcpDial>, &'p GrantListener),
}

impl Meets<'_> {
    /// Creates the sandbox that `create` describes where it meets the gateway, issued among
    /// `sockets` if at a socket, and returns what the create made and the registration that
    /// accepts the sandbox's session.
    fn create(
        self,
        sockets: &Sockets,
        create: &Create,
    ) -> Result<(Created, gateway::Registration), String> {
        let issued = |error: io::Error| error.to_string();
        match self {
            Self::Sockets(creator) => {
                let (dial, registration) = sockets.issue(&create.name).map_err(issued)?;
                Ok((creator.create(create, dial)?, registration))
            }
            Self::Grants(creator, listener) => {
                let (dial, registration) = listener.issue().map_err(issued)?;
                Ok((creator.create(create, dial)?, registration))
            }
        }
    }
}

/// One of a run's sandboxes, by which its start names it as no other sandbox of the run, and
/// finds its role.
#[derive(Clone, Copy)]
pub(super) enum Which {
    /// The agent's, `agent`, which a run starts once.
    Agent,
    /// Judge call `job`'s, `job-<number>`, of judge `judge`.
    Job(JobId, JudgeId),
    /// Lease `lease`'s, `lease-<number>`, of runner `runner`, as the given instance of its role.
    Lease(LeaseId, RunnerId, u32),
}

impl Which {
    fn role(self, workflow: &Workflow) -> RoleId {
        match self {
            Self::Agent => workflow.agent().role,
            Self::Job(_, judge) => judge.role(),
            Self::Lease(_, runner, _) => runner.role(),
        }
    }

    /// Which instance of its role it is ([`Create::instance`]): a lease's, or else the only one.
    fn instance(self) -> u32 {
        match self {
            Self::Agent | Self::Job(..) => 0,
            Self::Lease(_, _, instance) => instance,
        }
    }

    /// The event that it failed for `reason`.
    pub(super) fn failed(self, reason: String) -> Event {
        match self {
            Self::Agent => Event::AgentFailed { reason },
            Self::Job(job, _) => Event::JudgeFailed { job, reason },
            Self::Lease(lease, ..) => Event::LeaseFailed { lease, reason },
        }
    }

    /// Its directory in the run's record, which the record's layout names (`record`).
    pub(super) fn record_dir(self) -> String {
        match self {
            Self::Agent => "agent".to_owned(),
            Self::Job(job, _) => format!("jobs/{}", job.0),
            Self::Lease(lease, ..) => format!("leases/{}", lease.0),
        }
    }

    /// What it is, as messages about it name it, such as `the judge's` sandbox.
    pub(super) fn noun(self) -> &'static str {
        match self {
            Self::Agent => "the agent's",
            Self::Job(..) => "the judge's",
            Self::Lease(..) => "the runner's",
        }
    }

    /// The sandbox's name within its run, which its socket directory has too.
    pub(super) fn name(self) -> Name {
        let name = match self {
            Self::Agent => "agent".to_owned(),
            Self::Job(job, _) => format!("job-{}", job.0),
            Self::Lease(lease, ..) => format!("lease-{}", lease.0),
        };
        // A `Name` made of such parts, which couldn't fail, would be a `pub` item of the schema
        // for this one caller.
        let Ok(name) = Name::try_from(name) else {
            unreachable!(
                "a sandbox's name within its run is no name, though `agent`, and `job-` and \
                 `lease-` with a number's digits, are names: a letter, then letters, digits and \
                 `-`, 24 at most"
            )
        };
        name
    }
}

/// A sandbox that started, with its session, set up.
pub(super) struct Started {
    /// Which of the run's sandboxes it is, on the provider of whose role it runs.
    which: Which,
    id: SandboxId,
    pub(super) session: Session,
}

impl<'p> Published<'p> {
    /// Publishes what `placed`'s sandboxes run on each of `providers`, the providers its roles are
    /// placed on: `sandboxd`, a static binary, then each image a role placed there names, once;
    /// then makes the directory of the run's sockets in `record`, the run's record
    /// ([`record::sockets`]), which must not exist yet. The sandboxes of a private provider meet
    /// the gateway at `private` instead. Stops as interrupted before each publish once a signal
    /// has come.
    pub(super) fn create(
        providers: &ByProvider<&'p dyn Provider>,
        placed: &Placed,
        sandboxd: &Path,
        private: Option<&'p GrantListener>,
        record: &Path,
    ) -> Result<Self, Stopped> {
        let workflow = placed.workflow();
        // Each publish is a provider call that a stop doesn't cut short, and one may take
        // minutes, as a push does; so a signal is heeded before each, not after them all.
        let providers = placed.by_provider(|name| {
            interruption::check()?;
            let provider = *providers.get(name);
            let meets = match (provider.meeting(), private) {
                (Meeting::Local(creator), _) => Meets::Sockets(creator),
                (Meeting::Private(creator), Some(listener)) => Meets::Grants(creator, listener),
                (Meeting::Private(_), None) => unreachable!(
                    "provider `{name}` is private, and the run has no grant listener, though \
                     `Run::execute`'s caller binds one for every run that places a role on a \
                     private provider, as its doc requires"
                ),
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
        // In the record, so that they go where it goes: `record::create` made it, and checked
        // the path to it, so that no other user may rename entries in it, or above it, as
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
    /// The sandboxes of run `run`, of `placed`, which start from what was `published` for them,
    /// once the run has started on `host`: each start or delete keeps what it made in
    /// `record`, the run's, synced on `host`, and sends `logged` the run log's record of the GPUs
    /// each sandbox's `sandboxd` saw ([`log::Entry::Gpus`]) and of each delete
    /// ([`log::Entry::Deleted`]) but a started sandbox's ([`Sandboxes::delete`]).
    pub(super) fn new(
        published: Published<'p>,
        run: RunTag,
        placed: &'p Placed,
        record: &'p Path,
        host: &'p H,
        logged: Sender<Input>,
    ) -> Self {
        let Published {
            providers,
            images,
            sockets,
        } = published;
        // A start ends within a fortieth of its role's startup, or 15 s, and a list's time, of
        // its sandbox's end; so the looks follow the run's quickest startup.
        let between_looks = (placed.workflow().roles())
            .fold(Duration::from_secs(15), |between, (role, _)| {
                between.min(placed.startup(role) / 40)
            });
        Self {
            run,
            placed,
            providers,
            images,
            sockets,
            record,
            host,
            logged,
            created: Mutex::default(),
            between_looks,
        }
    }

    /// Starts `which` sandbox, its create given the run's `hard_end` ([`Create::hard_end`]), and
    /// accepts its session, set up, by `by`, its arrivals sent on `arrivals` as `wrap` makes them
    /// ([`Registration::accept`]); it makes ops of at most `ops`, and serves its role's model
    /// route, if it has one. A sandbox not started by then, found ended before it connects
    /// ([`Sandboxes::ended`]), or whose run a signal interrupts first, is deleted, its console
    /// kept and quoted in the error, with why the last list, the keep or the delete failed.
    ///
    /// [`Registration::accept`]: gateway::Registration::accept
    pub(super) fn start<T: Send + 'static>(
        &self,
        which: Which,
        ops: Option<OpPayloadLimit>,
        by: Deadline,
        hard_end: UnixTime,
        arrivals: Sender<T>,
        wrap: impl Fn(Arrival) -> T + Send + 'static,
    ) -> Result<Started, String> {
        let name = which.name();
        // The start's deadline by the machine's clock, which the provider reads, at most the hard
        // end, as the run is abandoned then.
        let now = UnixTime::of(SystemTime::now())?;
        let by_unix = (by.remaining()).map_or(now, |left| now.saturating_add(left));
        let role = which.role(self.workflow());
        let model = self.workflow().sandbox(role).model_route.is_some();
        let placement = self.placed.placement(role);
        let hosting = self.providers.get(&placement.provider);
        let create = Create {
            run: self.run.clone(),
            name,
            role: provider_role(self.workflow(), role),
            instance: which.instance(),
            class: placement.class.clone(),
            image: self.images.get(role).clone(),
            sandboxd: hosting.sandboxd.clone(),
            ops: ops.is_some(),
            model,
            hard_end,
            by: by_unix.min(hard_end),
        };
        let (Created { id, report }, registration) =
            hosting.meets.create(&self.sockets, &create)?;
        let made = Made {
            on: &placement.provider,
            id: id.clone(),
            which,
            report,
            gpus: None,
            kept: false,
        };
        let recorded = self.write_sandbox(&made, None);
        stdx::unpoisoned(self.created.lock()).push(made);
        if let Err(error) = recorded {
            return Err(self.unstarted(which, &id, &error));
        }
        let looks = Looks::new();
        let stopped = || {
            interruption::signal().is_some()
                || looks.ended(self.between_looks, || self.ended(hosting.provider, &id))
        };
        let accepted = registration.accept(by, ops, model, arrivals, wrap, stopped);
        let accepted = accepted.map_err(|error| match looks.found() {
            (true, _) => "it ended before it met the gateway: expected its `sandboxd` to run \
                          until it connects"
                .to_owned(),
            (false, Some(failed)) if error.kind() == io::ErrorKind::TimedOut => {
                format!("{error}; the last list of its provider failed: {failed}")
            }
            (false, _) => error.to_string(),
        });
        let session = accepted.and_then(|mut session| session.set_up(by).map(|()| session));
        let session = session.and_then(|session| match session.gpus() {
            Some(gpus) => self.saw(which, &id, gpus).map(|()| session),
            None => Ok(session),
        });
        match session {
            Ok(session) => Ok(Started { which, id, session }),
            Err(error) => Err(self.unstarted(which, &id, &error)),
        }
    }

    /// Records that `which` sandbox, `id`, saw `gpus` as it started: in its `sandbox.json`, and in
    /// the run log.
    fn saw(&self, which: Which, id: &SandboxId, gpus: &[GpuModel]) -> Result<(), String> {
        let on = self.on(which);
        let mut created = stdx::unpoisoned(self.created.lock());
        let made = (created.iter_mut()).filter(|made| made.on == on && made.id == *id);
        let made: Vec<Made<'_>> = made
            .map(|made| {
                made.gpus = Some(gpus.to_vec());
                made.clone()
            })
            .collect();
        // Written once the lock is let go, so that no other thread waits on the record's syncs.
        drop(created);
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

    /// Whether `provider`'s list of the run finds sandbox `id` ended: a list leaves it out only
    /// once it is deleted, or has ended and been freed ([`Sweep::list`]), so only a list that
    /// finds it running says it hasn't; or why the list failed.
    fn ended(&self, provider: &dyn Provider, id: &SandboxId) -> Result<bool, String> {
        let listed = provider.list(&self.run)?;
        Ok(!(listed.iter()).any(|listed| listed.id == *id && !listed.ended))
    }

    /// Why sandbox `id`, `which` of the run, whose session wasn't set up for `error`, didn't
    /// start: `error`, the end of its console, whether keeping its console failed, and whether
    /// deleting it failed, which this tries.
    fn unstarted(&self, which: Which, id: &SandboxId, error: &str) -> String {
        let provider = self.provider(which);
        let mut parts = vec![format!("sandbox {}: {error}", id.as_str())];
        match provider.console(&self.run, id) {
            Ok(console) => {
                let text = console.text.trim();
                if !text.is_empty() {
                    parts.push(format!("its console ends: {text}"));
                }
                let kept = self.write_console(which, id, &console);
                parts.extend(
                    kept.err()
                        .map(|error| format!("its console wasn't kept: {error}")),
                );
            }
            Err(error) => parts.push(format!("its console can't be read: {error}")),
        }
        let deleted = provider.delete(&self.run, std::slice::from_ref(id));
        let (logged, recorded) =
            self.record_deletes(self.on(which), std::slice::from_ref(id), &deleted);
        self.send(logged);
        if let Err(error) = deleted {
            parts.push(format!("deleting it failed: {error}"));
        }
        parts.extend(recorded.err());
        parts.join("; ")
    }

    pub(super) fn workflow(&self) -> &'p Workflow {
        self.placed.workflow()
    }

    /// The name of the provider that `which` sandbox's role is placed on.
    fn on(&self, which: Which) -> &'p Name {
        &self.placed.placement(which.role(self.workflow())).provider
    }

    /// The provider that `which` sandbox's role is placed on.
    fn provider(&self, which: Which) -> &'p dyn Provider {
        self.providers.get(self.on(which)).provider
    }

    /// Keeps the console of a sandbox that started ([`keep_console`](Self::keep_console)),
    /// deletes the sandbox, then ends its session. Returns the run log's records of it, which the
    /// thread that deletes it logs in its turn: the delete, if it succeeded, then why its console
    /// wasn't kept, if it wasn't, which fails nothing; and whether the delete failed. So the
    /// agent's teardown, which the loop performs, is logged as the loop performs it, before what
    /// the run's children report meanwhile.
    pub(super) fn delete(&self, started: Started) -> (Vec<log::Entry>, Result<(), String>) {
        let Started { which, id, session } = started;
        let unkept = self.keep_console(which, &id).err();
        let provider = self.provider(which);
        let deleted = provider.delete(&self.run, std::slice::from_ref(&id));
        drop(session);
        let (mut logged, recorded) =
            self.record_deletes(self.on(which), std::slice::from_ref(&id), &deleted);
        logged.extend(unkept.map(log::Entry::Note));
        let deleted = deleted.context("deleting the sandbox failed");
        (logged, super::failed_also(deleted, recorded))
    }

    /// Records in the run's record the sandbox that `made` names, as [`record::write_sandbox`]
    /// does, with `deleted`, the outcome of its latest delete, if it has had one.
    fn write_sandbox(
        &self,
        made: &Made<'_>,
        deleted: Option<&Result<(), String>>,
    ) -> Result<(), String> {
        let Made {
            on,
            id,
            which,
            report,
            gpus,
            kept: _,
        } = made;
        let dir = self.record.join(which.record_dir());
        record::write_sandbox(self.host, &dir, on, id, report, gpus.as_deref(), deleted)
    }

    /// Records `deleted`, the outcome of a delete of sandboxes `ids` on provider `on`, for each
    /// of them that a start of the run created. Returns, if it succeeded, the run log's record of
    /// each one's delete, and whether recording failed.
    fn record_deletes(
        &self,
        on: &Name,
        ids: &[SandboxId],
        deleted: &Result<(), String>,
    ) -> (Vec<log::Entry>, Result<(), String>) {
        let created = stdx::unpoisoned(self.created.lock());
        let made = (created.iter()).filter(|made| made.on == on && ids.contains(&made.id));
        // Written once the lock is let go, so that no other thread waits on the record's syncs.
        let made: Vec<Made<'_>> = made.map(Made::clone).collect();
        drop(created);
        let mut logged = Vec::new();
        if deleted.is_ok() {
            for id in ids {
                let made = made.iter().find(|made| made.id == *id);
                let deleted = Deleted {
                    sandbox: made.map(|made| made.which.name()),
                    provider: on.clone(),
                    id: id.clone(),
                };
                logged.push(log::Entry::Deleted(deleted));
            }
        }
        let failures: Vec<String> = (made.iter())
            .filter_map(|made| self.write_sandbox(made, Some(deleted)).err())
            .collect();
        let recorded = if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        };
        (logged, recorded)
    }

    /// Sends `logged`, records for the run log, into the loop's inbox.
    fn send(&self, logged: Vec<log::Entry>) {
        for entry in logged {
            // The loop's inbox outlives every thread that deletes a sandbox.
            let _ = self.logged.send(Input::Logged(entry));
        }
    }

    /// Keeps sandbox `id`'s console, `which` of the run, as its provider shows it, in the run's
    /// record ([`record::write_console`]).
    ///
    /// # Errors
    ///
    /// Fails if the provider can't show the console, or the record can't be written.
    fn keep_console(&self, which: Which, id: &SandboxId) -> Result<(), String> {
        let read = self.provider(which).console(&self.run, id);
        let unkept = format_args!("sandbox {}'s console wasn't kept", id.as_str());
        let console = read.context(unkept)?;
        self.write_console(which, id, &console).context(unkept)
    }

    /// Writes `console`, that of sandbox `id`, `which` of the run, in the run's record, and once
    /// written, no longer counts the sandbox among those whose console the sweep keeps.
    fn write_console(&self, which: Which, id: &SandboxId, console: &Console) -> Result<(), String> {
        record::write_console(
            self.host,
            &self.record.join(which.record_dir()),
            &console.text,
        )?;
        // A later read could only replace what was kept with less: a sandbox being deleted, as an
        // EC2 instance being terminated, may show nothing.
        let on = self.on(which);
        let mut created = stdx::unpoisoned(self.created.lock());
        for made in (created.iter_mut()).filter(|made| made.on == on && made.id == *id) {
            made.kept = true;
        }
        Ok(())
    }

    /// Deletes, on each of the run's providers, every sandbox of the run that it still lists
    /// ([`sweep`]), keeping first the console of each that a start of the run created, unless an
    /// earlier read kept it ([`keep_console`](Self::keep_console)). Returns why each console it
    /// was to keep wasn't, for the run log, and why a provider's list or delete failed, if one
    /// did.
    pub(super) fn sweep(&self) -> (Vec<String>, Result<(), String>) {
        let mut unkept = Vec::new();
        let swept = (self.providers.iter()).map(|(name, hosting)| {
            let keep = |id: &SandboxId| {
                let created = stdx::unpoisoned(self.created.lock());
                let which = (created.iter().rev())
                    .find(|made| made.on == name && made.id == *id && !made.kept)
                    .map(|made| made.which);
                drop(created);
                if let Some(which) = which {
                    unkept.extend(self.keep_console(which, id).err());
                }
            };
            let record = |ids: &[SandboxId], deleted: &Result<(), String>| {
                let (logged, recorded) = self.record_deletes(name, ids, deleted);
                self.send(logged);
                recorded
            };
            sweep(name, hosting.provider, &self.run, keep, record).err()
        });
        let failures: Vec<String> = swept.flatten().collect();
        let swept = if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        };
        (unkept, swept)
    }
}

/// A sandbox of the run that a delete ended, as the run log records it
/// ([`log::Entry::Deleted`]).
pub(super) struct Deleted {
    /// Its name within the run ([`Which::name`]), if a start of the run created it.
    pub(super) sandbox: Option<Name>,
    /// The provider it was on.
    pub(super) provider: Name,
    pub(super) id: SandboxId,
}

/// Deletes every sandbox of run `run` that `provider`, the provider named `name`, still lists,
/// calling `before` with each first, and `after` with them all and the delete's outcome, and
/// returns their ids, or why the list, the delete or `after` failed.
pub(super) fn sweep(
    name: &Name,
    provider: &dyn Sweep,
    run: &RunTag,
    mut before: impl FnMut(&SandboxId),
    after: impl FnOnce(&[SandboxId], &Result<(), String>) -> Result<(), String>,
) -> Result<Vec<SandboxId>, String> {
    let listed = provider.list(run).context(format_args!(
        "listing the run's sandboxes on provider `{name}` failed"
    ))?;
    let ids: Vec<SandboxId> = (listed.into_iter())
        .map(|Listed { id, ended: _ }| id)
        .collect();
    for id in &ids {
        before(id);
    }
    if !ids.is_empty() {
        let deleted = provider.delete(run, &ids);
        let recorded = after(&ids, &deleted);
        let deleted = deleted.context(format_args!(
            "deleting the sandboxes the run left on provider `{name}` failed"
        ));
        super::failed_also(deleted, recorded)?;
    }
    Ok(ids)
}

/// A start's looks for its sandbox's end, made from within its accept's wait, each once the time
/// between looks has passed since the last.
struct Looks {
    last: Cell<Instant>,
    /// Whether a look found the sandbox ended.
    ended: Cell<bool>,
    /// Why the last look's list failed, if it did.
    failed: RefCell<Option<String>>,
}

impl Looks {
    fn new() -> Self {
        Self {
            last: Cell::new(Instant::now()),
            ended: Cell::new(false),
            failed: RefCell::new(None),
        }
    }

    /// Whether a look found the sandbox ended, looking with `look` if `between` has passed since
    /// the last look. The first comes `between` after the create returned, so every list speaks
    /// for the sandbox it made.
    fn ended(&self, between: Duration, look: impl FnOnce() -> Result<bool, String>) -> bool {
        if !self.ended.get() && self.last.get().elapsed() >= between {
            match look() {
                Ok(ended) => {
                    self.failed.replace(None);
                    self.ended.set(ended);
                }
                Err(error) => {
                    self.failed.replace(Some(error));
                }
            }
            self.last.set(Instant::now());
        }
        self.ended.get()
    }

    /// Whether a look found the sandbox ended, and why the last list failed, if it did.
    fn found(self) -> (bool, Option<String>) {
        (self.ended.get(), self.failed.into_inner())
    }
}

/// How to start `command` in a sandbox: as the IR says, in [`command_env()`]'s environment, with
/// an empty standard input, and its output at [`command_stdout()`] and [`command_stderr()`].
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

/// Collects the sandbox's `dir`, below [`pumpkin_dir()`], into the same path below `record`, a
/// new directory there, within [`PUMPKIN`], by `by`. After a failure, it holds what arrived
/// before it.
pub(super) fn collect(
    session: &mut Session,
    dir: &RelativePath,
    record: &Path,
    by: Deadline,
) -> Result<(), String> {
    let from = pumpkin_dir().join(dir);
    session
        .collect(&from, &record.join(dir.as_str()), PUMPKIN, by)
        .context(format_args!("collecting {from} failed"))
}
