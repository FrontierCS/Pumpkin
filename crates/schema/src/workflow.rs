//! A workflow parsed from its IR: references resolved to typed ids, every time given as a
//! [`Duration`], and the rules that span its parts checked.
//!
//! A [`Placed`] workflow has each of its roles on a provider, in the smallest class of sandbox
//! offered that fits it. A [`Workflow`] can only be made by [`Workflow::parse`], and a [`Placed`]
//! by [`Workflow::place`], or by [`Placed::built`] from one, so code that acts on one need not
//! check again what they established.
//! Each kind of part has its own id type, so that, for example, the role a [`JobFunction`]
//! targets is known to be a [`Target`]. Ids are only valid for the workflow that issued them.

use std::collections::{BTreeMap, BTreeSet};
use std::num::{NonZeroU32, NonZeroU64};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ir::{
    AbsolutePath, Command, CpuFlag, CpuModel, Dir, Env, File, Gpu, Image, JsonPointer, ModelRoute,
    Name, Needs, TargetLifetime, User, WorkflowName,
};
use crate::sandbox::OpPayloadLimit;

mod errors;
mod ops;
mod parse;
mod place;

pub use errors::{ParseError, ParseErrors};

/// A workflow that parsing checked: only parsing makes one, so code past it needn't check again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workflow {
    ir: crate::ir::Workflow,
    /// Each role's sandbox, by its id.
    roles: Vec<Sandbox>,
    agents: BTreeMap<AgentId, Agent>,
    targets: BTreeMap<RoleId, Target>,
    runners: BTreeMap<RoleId, Runner>,
    job_functions: Vec<JobFunction>,
    sessions: Vec<Session>,
    now_functions: Vec<NowFunction>,
    final_call: Final,
}

impl Workflow {
    pub fn ir(&self) -> &crate::ir::Workflow {
        &self.ir
    }

    pub fn name(&self) -> &WorkflowName {
        &self.ir.name
    }

    /// Each agent in the IR's role order.
    pub fn agents(&self) -> impl ExactSizeIterator<Item = &Agent> {
        self.agents.values()
    }

    pub fn agent_by_id(&self, id: AgentId) -> &Agent {
        &self.agents[&id]
    }

    pub fn work_deadline(&self) -> Duration {
        Duration::from_secs(self.ir.deadline_seconds.get())
    }

    pub fn target(&self, id: TargetId) -> &Target {
        &self.targets[&id.0]
    }

    /// Targets whose sandbox spans the work phase, in declaration order.
    pub fn work_targets(&self) -> impl Iterator<Item = TargetId> {
        self.targets.iter().filter_map(|(role, target)| {
            (target.lifetime == TargetLifetime::Work).then_some(TargetId(*role))
        })
    }

    pub fn runner(&self, id: RunnerId) -> &Runner {
        &self.runners[&id.0]
    }

    pub fn sandbox(&self, role: RoleId) -> &Sandbox {
        &self.roles[role.0]
    }

    /// The model route of `role`, the agent's or a target's, with the budgets of its entries, if
    /// it has one.
    pub fn model(&self, role: RoleId) -> Option<&ModelRoute> {
        if let Some(agent) = self.agents.get(&AgentId(role)) {
            return agent.model.as_ref();
        }
        let target = self.targets.get(&role).map(|target| &target.model);
        target.and_then(Option::as_ref)
    }

    /// Each role, by its id, and its sandbox, in the IR's order.
    pub fn roles(&self) -> impl Iterator<Item = (RoleId, &Sandbox)> {
        (self.roles.iter().enumerate()).map(|(index, sandbox)| (RoleId(index), sandbox))
    }

    /// What `f` makes of each role, by its id, and its sandbox, in [`roles`](Self::roles)' order.
    /// Fails with `f`'s first error, calling `f` on no role after it.
    pub fn by_role<T, E>(
        &self,
        f: impl FnMut((RoleId, &Sandbox)) -> Result<T, E>,
    ) -> Result<ByRole<T>, E> {
        self.roles().map(f).collect::<Result<_, _>>().map(ByRole)
    }

    /// Each agent's job functions, in the IR's order.
    pub fn job_functions(&self) -> impl Iterator<Item = (JobFunctionId, &JobFunction)> {
        let functions = self.job_functions.iter().enumerate();
        functions.map(|(index, function)| (JobFunctionId(index), function))
    }

    pub fn job_functions_of(
        &self,
        role: RoleId,
    ) -> impl Iterator<Item = (JobFunctionId, &JobFunction)> {
        self.job_functions()
            .filter(move |(_, function)| function.holder.role() == role)
    }

    pub fn job_function(&self, id: JobFunctionId) -> &JobFunction {
        &self.job_functions[id.0]
    }

    /// Every role's sessions: each agent's, then each target's, each in the IR's order.
    pub fn sessions(&self) -> impl Iterator<Item = (SessionId, &Session)> {
        (self.sessions.iter().enumerate()).map(|(index, grant)| (SessionId(index), grant))
    }

    pub fn session(&self, id: SessionId) -> &Session {
        &self.sessions[id.0]
    }

    /// Each agent's now functions, in the IR's order.
    pub fn now_functions(&self) -> impl Iterator<Item = (NowFunctionId, &NowFunction)> {
        (self.now_functions.iter().enumerate()).map(|(index, now)| (NowFunctionId(index), now))
    }

    pub fn now_functions_of(
        &self,
        role: RoleId,
    ) -> impl Iterator<Item = (NowFunctionId, &NowFunction)> {
        self.now_functions()
            .filter(move |(_, function)| function.holder.role() == role)
    }

    pub fn now_function(&self, id: NowFunctionId) -> &NowFunction {
        &self.now_functions[id.0]
    }

    /// How submissions are judged.
    pub fn final_call(&self) -> &Final {
        &self.final_call
    }
}

/// A parsed workflow whose every role is placed on a provider, in a class of sandbox that the
/// provider offers, and that the role fits ([`Workflow::place`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    workflow: Workflow,
    placements: ByRole<Placement>,
}

impl Placed {
    pub fn workflow(&self) -> &Workflow {
        &self.workflow
    }

    pub fn placement(&self, role: RoleId) -> &Placement {
        self.placements.get(role)
    }

    /// The same placements, with `built` as the workflow: the placed workflow parsed again with
    /// its images built, so with the same roles and needs.
    ///
    /// # Panics
    ///
    /// `built`'s roles, or their needs, are not those of the placed workflow.
    #[must_use]
    pub fn built(self, built: Workflow) -> Self {
        let placed = self.workflow.roles.iter().map(|sandbox| &sandbox.needs);
        assert!(
            placed.eq(built.roles.iter().map(|sandbox| &sandbox.needs)),
            "a workflow with its images built has other roles or needs than it was placed with"
        );
        Self {
            workflow: built,
            placements: self.placements,
        }
    }

    /// The longest a start of one of `role`'s sandboxes takes: its class's
    /// [`startup_seconds`](Class::startup_seconds).
    pub fn startup(&self, role: RoleId) -> Duration {
        Duration::from_secs(self.placement(role).declared.startup_seconds.get())
    }

    /// What `f` makes of each provider that a role is placed on, by its name, in the order of
    /// their names. Fails with `f`'s first error, calling `f` on no provider after it.
    pub fn by_provider<T, E>(
        &self,
        mut f: impl FnMut(&Name) -> Result<T, E>,
    ) -> Result<ByProvider<T>, E> {
        let placements = self.placements.0.iter();
        let providers: BTreeSet<&Name> = placements.map(|placement| &placement.provider).collect();
        let made = (providers.into_iter()).map(|provider| Ok((provider.clone(), f(provider)?)));
        Ok(ByProvider(made.collect::<Result<_, _>>()?))
    }
}

/// Where a role is placed, or may be: in a class of sandbox that a provider declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    /// The provider, by the name the run knows it by.
    pub provider: Name,
    /// The class, by the name the provider declares it by (`provider::Ready::classes`).
    pub class: Name,
    pub declared: Class,
}

/// A class of sandbox that a provider creates, such as a size of machine, and what a run may
/// rely on of its sandboxes, which a role is placed in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Class {
    pub offers: Offers,
    /// The most seconds a sandbox's start takes, from the start of its create until its
    /// `sandboxd` has met the gateway and its session is set up.
    pub startup_seconds: NonZeroU64,
    /// What a sandbox of the class costs an hour, as the operator's settings give it, if they do.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usd_per_hour: Option<UsdPerHour>,
}

/// A price in US dollars an hour, to the millionth of a dollar, such as `0.3672`, from 0 to a
/// million: what an operator says a sandbox costs, which a run's record keeps beside the time it
/// ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct UsdPerHour {
    micros: u64,
}

impl TryFrom<f64> for UsdPerHour {
    type Error = String;

    fn try_from(usd: f64) -> Result<Self, Self::Error> {
        if !(0.0..=1e6).contains(&usd) {
            return Err(format!(
                "{usd} is not a price in dollars an hour: give one from 0 to 1000000, such as 0.3672"
            ));
        }
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "from 0 to 10^12, as just checked"
        )]
        let micros = (usd * 1e6).round() as u64;
        Ok(Self { micros })
    }
}

impl From<UsdPerHour> for f64 {
    fn from(price: UsdPerHour) -> Self {
        // A million dollars in millionths is exact as a double.
        #[expect(clippy::cast_precision_loss, reason = "at most 10^12, below 2^53")]
        let micros = price.micros as Self;
        micros / 1e6
    }
}

/// What a class of sandbox offers the roles of its sandboxes, as its provider states it.
///
/// It is the most CPU and memory that a role may need, its GPUs, and its CPU's flags and model,
/// which a role's [needs](crate::ir::Needs) are matched against
/// ([`Workflow::place`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Offers {
    /// In thousandths of a CPU.
    pub cpu_millis: NonZeroU64,
    pub memory_bytes: NonZeroU64,
    /// None if its sandboxes have no GPU.
    pub gpu: Option<Gpu>,
    pub cpu_features: BTreeSet<CpuFlag>,
    /// None if the provider states no model.
    pub cpu_model: Option<CpuModel>,
}

/// A value for each provider that a [`Placed`] workflow places a role on: what its placing gave
/// of each ([`Workflow::place`]), or what [`Placed::by_provider`] made of each.
#[derive(Debug)]
pub struct ByProvider<T>(BTreeMap<Name, T>);

impl<T> ByProvider<T> {
    /// The value for `provider`, a provider that the placed workflow places a role on.
    pub fn get(&self, provider: &Name) -> &T {
        &self.0[provider]
    }

    /// Each provider's value, by the provider's name, in the order of their names.
    pub fn iter(&self) -> impl Iterator<Item = (&Name, &T)> {
        self.0.iter()
    }

    pub fn map<U>(self, mut f: impl FnMut(T) -> U) -> ByProvider<U> {
        let mapped = self.0.into_iter().map(|(name, value)| (name, f(value)));
        ByProvider(mapped.collect())
    }
}

/// A role of any kind, by its place among the IR's roles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RoleId(usize);

impl RoleId {
    /// The role's index in the workflow's IR declaration order.
    pub fn index(self) -> usize {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentId(RoleId);

impl AgentId {
    pub fn role(self) -> RoleId {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TargetId(RoleId);

impl TargetId {
    pub fn role(self) -> RoleId {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RunnerId(RoleId);

impl RunnerId {
    pub fn role(self) -> RoleId {
        self.0
    }
}

/// A value for each role of the workflow that made it, by the role's id
/// ([`Workflow::by_role`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ByRole<T>(Vec<T>);

impl<T> ByRole<T> {
    /// The value for `role`, a role of the workflow that made this.
    pub fn get(&self, role: RoleId) -> &T {
        &self.0[role.0]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobFunctionId(usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NowFunctionId(usize);

/// What a job runs: a call of one of the agent's job functions, or the workflow's [`Final`] call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JobOf {
    Function(JobFunctionId),
    Final,
}

/// What every role has: the sandbox its instances run in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sandbox {
    pub name: Name,
    pub image: Image,
    pub user: User,
    pub needs: Needs,
    /// The role's place in the IR, where placing it reports what is wrong.
    pub(crate) at: JsonPointer,
}

/// The agent role; see [`ir::AgentRuntime`](crate::ir::AgentRuntime), whose rules its `dirs`
/// and `files` keep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent {
    pub id: AgentId,
    pub command: Command,
    /// In order of their paths.
    pub dirs: Vec<Dir>,
    pub files: Vec<File>,
    pub stdin: Option<String>,
    /// See
    /// [`ir::AgentRuntime::finalize_grace_seconds`](crate::ir::AgentRuntime::finalize_grace_seconds).
    pub finalize_grace: Duration,
    /// The agent's model route, with the budgets of its entries, if it has one.
    pub model: Option<ModelRoute>,
}

/// A target role; see [`ir::TargetRuntime`](crate::ir::TargetRuntime).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub lifetime: TargetLifetime,
    pub cwd: AbsolutePath,
    pub env: Env,
    /// The target's model route, with the budgets of its entries, if it has one.
    pub model: Option<ModelRoute>,
}

/// A runner role; see [`ir::RunnerRuntime`](crate::ir::RunnerRuntime).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Runner {
    pub cwd: AbsolutePath,
    pub env: Env,
}

/// A job function of the agent's; see [`ir::JobFunction`](crate::ir::JobFunction).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobFunction {
    pub help: String,
    pub holder: AgentId,
    pub name: Name,
    /// The target that each call starts afresh.
    pub target: TargetId,
    pub body: crate::ir::Body,
    pub keywords: serde_json::Map<String, serde_json::Value>,
    /// At most [`MAX_OP_PAYLOAD_BYTES`](crate::sandbox::MAX_OP_PAYLOAD_BYTES), as parsing checks.
    pub(crate) payload_max_bytes: NonZeroU64,
    pub calls: Option<NonZeroU64>,
    pub cumulative_seconds: Option<NonZeroU64>,
    pub call_timeout_seconds: NonZeroU64,
}

impl JobFunction {
    /// [`ir::JobFunction::cumulative_seconds`](crate::ir::JobFunction::cumulative_seconds), as a
    /// [`Duration`].
    pub fn cumulative(&self) -> Option<Duration> {
        self.cumulative_seconds
            .map(|seconds| Duration::from_secs(seconds.get()))
    }

    /// [`ir::JobFunction::call_timeout_seconds`](crate::ir::JobFunction::call_timeout_seconds),
    /// as a [`Duration`].
    pub fn call_timeout(&self) -> Duration {
        Duration::from_secs(self.call_timeout_seconds.get())
    }

    pub fn payload_max_bytes(&self) -> NonZeroU64 {
        self.payload_max_bytes
    }

    /// The op payload limit of a session whose payloads the function takes.
    pub fn payload_limit(&self) -> OpPayloadLimit {
        OpPayloadLimit(self.payload_max_bytes.get())
    }
}

/// A session; see [`ir::Session`](crate::ir::Session).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub help: String,
    pub name: Name,
    pub holder: Holder,
    /// An agent's runner has an image no target has
    /// ([`ir::Session::runner`](crate::ir::Session::runner)).
    pub runner: RunnerId,
    pub instances: NonZeroU32,
    pub instance_seconds: NonZeroU64,
    pub cumulative_seconds: Option<NonZeroU64>,
    pub commands: Option<NonZeroU64>,
    pub transfer_bytes: NonZeroU64,
}

/// The role that holds a [`Session`]: the agent, or a target, each of whose calls uses it afresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holder {
    Agent(AgentId),
    Target(TargetId),
}

impl Holder {
    pub fn role(self) -> RoleId {
        match self {
            Self::Agent(agent) => agent.role(),
            Self::Target(target) => target.role(),
        }
    }
}

impl Session {
    /// [`ir::Session::instance_seconds`](crate::ir::Session::instance_seconds), as a
    /// [`Duration`].
    pub fn instance(&self) -> Duration {
        Duration::from_secs(self.instance_seconds.get())
    }

    /// [`ir::Session::cumulative_seconds`](crate::ir::Session::cumulative_seconds), as a
    /// [`Duration`].
    pub fn cumulative(&self) -> Option<Duration> {
        (self.cumulative_seconds).map(|seconds| Duration::from_secs(seconds.get()))
    }
}

/// One of the agent's now functions; see [`ir::NowFunction`](crate::ir::NowFunction).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NowFunction {
    pub help: String,
    pub holder: AgentId,
    pub name: Name,
    pub body: crate::ir::Body,
    pub payload_max_bytes: NonZeroU64,
}

impl NowFunction {
    /// The op payload limit of a session whose payloads the function takes.
    pub fn payload_limit(&self) -> OpPayloadLimit {
        OpPayloadLimit(self.payload_max_bytes.get())
    }
}

/// The parsed [`ir::Final`](crate::ir::Final): a body on a target that no job function names,
/// whose `payload_max_bytes` caps every submission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Final {
    pub target: TargetId,
    pub body: crate::ir::FinalBody,
    pub keywords: serde_json::Map<String, serde_json::Value>,
    /// At most [`MAX_OP_PAYLOAD_BYTES`](crate::sandbox::MAX_OP_PAYLOAD_BYTES), as parsing checks.
    pub(crate) payload_max_bytes: NonZeroU64,
    /// See [`ir::Final::timeout_seconds`](crate::ir::Final::timeout_seconds).
    pub timeout: Duration,
}

impl Final {
    /// See [`ir::Final::payload_max_bytes`](crate::ir::Final::payload_max_bytes).
    pub fn payload_max_bytes(&self) -> NonZeroU64 {
        self.payload_max_bytes
    }
}
