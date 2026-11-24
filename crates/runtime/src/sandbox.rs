//! The one path every sandbox of a run starts on, and where each is recorded and deleted.
//!
//! [`Published`] holds what a run puts on its providers before the first start. Every start,
//! delete and disposal runs on the driver's loop (`starts`), which remembers each sandbox it
//! created, so that a delete is recorded against the sandbox's own record; a provider's own calls
//! run on a thread of their own, which completes into the loop. A child's thread asks the loop
//! through [`Sandboxes`], and waits. [`sweep`] deletes whatever a provider still lists of the run.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::num::NonZeroU32;
use std::path::Path;
use std::time::Duration;

use decision::{JobId, LeaseId};
use gateway::{Dialer, Listener, Sockets};
use provider::{
    Admits, Created, Creator, ImageRef, Listed, Meeting, Provider, Role, RunTag, SandboxId,
    SandboxdRef, Sweep, Undeleted,
};
use schema::ir::{Command, GpuModel, Image, Name};
use schema::sandbox::tree::Limits;
use schema::sandbox::{
    Endpoint, Launch, Listening, PUMPKIN_BYTES, PrivateDial, SocketDial, command_env,
    command_stderr, command_stdout, pumpkin_dir,
};
use schema::workflow::{
    AgentId, ByProvider, ByRole, JobFunctionId, Placed, RoleId, RunnerId, SessionId, TargetId,
    Workflow,
};
use serde::{Deserialize, Deserializer, de};
use stdx::Context;

use super::{Stopped, interruption, record};

/// The most entries, files and directories, that the host collects from a sandbox in one tree.
pub(super) const ENTRIES: u64 = 100_000;

// What a sandbox's `/pumpkin` holds at most: anything collected from below it fits.
pub(super) const PUMPKIN: Limits = Limits {
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

    /// How many operations the run's loop holds in flight at most: for each sandbox that the run's
    /// phases hold at once, a receive and a send on its session, an accept as it meets the gateway,
    /// and a sync of its record; and a sync of the run log and of the payloads.
    pub(super) fn operations(&self) -> usize {
        let sandboxes = |phase: &BTreeMap<RoleId, NonZeroU32>| -> usize {
            phase.values().map(|count| count.get() as usize).sum()
        };
        let most = sandboxes(&self.work).max(sandboxes(&self.final_phase));
        // A phase's sandboxes, and the ones whose deletes the next phase's starts may overlap.
        4 * (2 * most) + 2
    }

    pub(super) fn instances(&self, role: RoleId) -> NonZeroU32 {
        self.work
            .get(&role)
            .into_iter()
            .chain(self.final_phase.get(&role))
            .copied()
            .max()
            .unwrap_or(NonZeroU32::MIN)
    }
}

pub(super) fn provider_role(workflow: &Workflow, role: RoleId, instances: NonZeroU32) -> Role {
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

/// What every start of the run's sandboxes reads, and how a thread asks the loop for one.
pub(super) struct Sandboxes<'p> {
    pub(super) run: RunTag,
    pub(super) capacity: &'p Capacity,
    pub(super) placed: &'p Placed,
    pub(super) providers: ByProvider<Hosting<'p>>,
    pub(super) images: ByRole<ImageRef>,
    pub(super) sockets: Sockets,
    pub(super) between_looks: Duration,
}

/// The sandboxes that the run's starts created, which the loop alone holds.
#[derive(Default)]
pub(super) struct Inventory<'p> {
    pub(super) created: Vec<Made<'p>>,
    pub(super) unnamed: BTreeSet<&'p Name>,
}

#[derive(Clone)]
pub(super) struct Made<'p> {
    pub(super) on: &'p Name,
    pub(super) id: SandboxId,
    pub(super) which: Which,
    pub(super) deleted: bool,
    pub(super) gpus: Option<Vec<GpuModel>>,
}

pub(super) struct Hosting<'p> {
    pub(super) provider: &'p dyn Provider,
    pub(super) sandboxd: SandboxdRef,
    pub(super) meets: Meets<'p>,
}

#[derive(Clone, Copy)]
pub(super) enum Meets<'p> {
    Sockets(&'p dyn Creator<SocketDial>),
    Private(
        &'p dyn Creator<PrivateDial, (Created, Admits)>,
        &'p Listener,
    ),
    Dials(&'p dyn Creator<Listening, (Created, Endpoint)>, &'p Dialer),
}

#[derive(Clone, Copy)]
pub(super) enum Which {
    Agent(AgentId),
    Shared(TargetId),
    Call(JobId, TargetId, u32),
    Lease(LeaseId, RunnerId, u32),
}

impl Which {
    pub(super) fn role(self) -> RoleId {
        match self {
            Self::Agent(agent) => agent.role(),
            Self::Shared(target) | Self::Call(_, target, _) => target.role(),
            Self::Lease(_, runner, _) => runner.role(),
        }
    }

    pub(super) fn instance(self) -> u32 {
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

impl<'p> Sandboxes<'p> {
    pub(super) fn new(
        published: Published<'p>,
        (run, placed, capacity): (RunTag, &'p Placed, &'p Capacity),
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
            between_looks,
        }
    }

    pub(super) fn workflow(&self) -> &'p Workflow {
        self.placed.workflow()
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
