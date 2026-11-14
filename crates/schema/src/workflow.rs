//! A workflow parsed from its IR: references resolved to typed ids, every time given as a
//! [`Duration`], and the rules that span its parts checked.
//!
//! A [`Placed`] workflow has each of its roles on a provider, in the smallest class of sandbox
//! offered that fits it. A [`Workflow`] can only be made by [`Workflow::parse`], and a [`Placed`] by
//! [`Workflow::place`], so code that acts on one need not check again what they established.
//! Each kind of part has its own id type, so that, for example, the role a [`JudgeGrant`]
//! judges with is known to be a [`Judge`]. Ids are only valid for the workflow that issued them.

use std::collections::{BTreeMap, BTreeSet};
use std::num::{NonZeroU32, NonZeroU64};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ir::{
    AbsolutePath, Command, CpuFlag, CpuModel, Dir, Env, File, Gpu, Image, JsonPointer, ModelRoute,
    Name, Needs, ResultSpec, User, WorkflowName,
};
use crate::sandbox::OpPayloadLimit;

mod errors;
mod parse;
mod place;

pub use errors::{ParseError, ParseErrors};

/// A parsed workflow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workflow {
    ir: crate::ir::Workflow,
    /// Each role's sandbox, by its id.
    roles: Vec<Sandbox>,
    agent: Agent,
    judges: BTreeMap<RoleId, Judge>,
    runners: BTreeMap<RoleId, Runner>,
    judge_grants: Vec<JudgeGrant>,
    lease_grants: Vec<LeaseGrant>,
    submit_grant: SubmitGrant,
    score: Score,
}

impl Workflow {
    /// The IR that this workflow was parsed from.
    pub fn ir(&self) -> &crate::ir::Workflow {
        &self.ir
    }

    pub fn name(&self) -> &WorkflowName {
        &self.ir.name
    }

    /// The workflow's one agent.
    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    pub fn judge(&self, id: JudgeId) -> &Judge {
        &self.judges[&id.0]
    }

    pub fn runner(&self, id: RunnerId) -> &Runner {
        &self.runners[&id.0]
    }

    /// The sandbox of any role.
    pub fn sandbox(&self, role: RoleId) -> &Sandbox {
        &self.roles[role.0]
    }

    /// Each role, by its id, and its sandbox, in the IR's order.
    pub fn roles(&self) -> impl Iterator<Item = (RoleId, &Sandbox)> {
        (self.roles.iter().enumerate()).map(|(index, sandbox)| (RoleId(index), sandbox))
    }

    /// What `f` makes of each role, by its id, and its sandbox, in [`roles`](Self::roles)' order.
    ///
    /// # Errors
    ///
    /// Fails with `f`'s first error, calling `f` on no role after it.
    pub fn by_role<T, E>(
        &self,
        f: impl FnMut((RoleId, &Sandbox)) -> Result<T, E>,
    ) -> Result<ByRole<T>, E> {
        self.roles().map(f).collect::<Result<_, _>>().map(ByRole)
    }

    pub fn slot(&self, id: SlotId) -> &Slot {
        &self.judge(id.judge).slots[id.index]
    }

    /// The agent's judge grants, in the IR's order.
    pub fn judge_grants(&self) -> impl Iterator<Item = (JudgeGrantId, &JudgeGrant)> {
        self.judge_grants
            .iter()
            .enumerate()
            .map(|(index, grant)| (JudgeGrantId(index), grant))
    }

    pub fn judge_grant(&self, id: JudgeGrantId) -> &JudgeGrant {
        &self.judge_grants[id.0]
    }

    /// The agent's lease grants, in the IR's order.
    pub fn lease_grants(&self) -> impl Iterator<Item = (LeaseGrantId, &LeaseGrant)> {
        (self.lease_grants.iter().enumerate()).map(|(index, grant)| (LeaseGrantId(index), grant))
    }

    pub fn lease_grant(&self, id: LeaseGrantId) -> &LeaseGrant {
        &self.lease_grants[id.0]
    }

    /// The agent's one submit grant.
    pub fn submit_grant(&self) -> &SubmitGrant {
        &self.submit_grant
    }

    /// How the selected submission is judged.
    pub fn score(&self) -> &Score {
        &self.score
    }

    /// The slot a judge call's payload lands in.
    pub fn call_slot(&self, call: JudgeCall) -> SlotId {
        match call {
            JudgeCall::Grant(grant) => self.judge_grant(grant).slot,
            JudgeCall::Score => self.score.slot,
        }
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

    /// Where `role` is placed.
    pub fn placement(&self, role: RoleId) -> &Placement {
        self.placements.get(role)
    }

    /// The longest a start of one of `role`'s sandboxes takes: its class's
    /// [`startup_seconds`](Class::startup_seconds).
    pub fn startup(&self, role: RoleId) -> Duration {
        Duration::from_secs(self.placement(role).declared.startup_seconds.get())
    }

    /// What `f` makes of each provider that a role is placed on, by its name, in the order of
    /// their names.
    ///
    /// # Errors
    ///
    /// Fails with `f`'s first error, calling `f` on no provider after it.
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
    /// What the provider declares of the class.
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

    /// What `f` makes of each provider's value, for the same providers.
    pub fn map<U>(self, mut f: impl FnMut(T) -> U) -> ByProvider<U> {
        let mapped = self.0.into_iter().map(|(name, value)| (name, f(value)));
        ByProvider(mapped.collect())
    }
}

/// A role of any kind, by its place among the IR's roles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RoleId(usize);

/// A judge role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JudgeId(RoleId);

impl JudgeId {
    pub fn role(self) -> RoleId {
        self.0
    }
}

/// A runner role.
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

/// A slot of a judge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotId {
    judge: JudgeId,
    index: usize,
}

impl SlotId {
    /// The judge that has the slot.
    pub fn judge(self) -> JudgeId {
        self.judge
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JudgeGrantId(usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeaseGrantId(usize);

/// What a judge call is made under: one of the agent's judge grants, or the workflow's
/// [`Score`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JudgeCall {
    Grant(JudgeGrantId),
    Score,
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
    /// Only the agent's may be set.
    pub model_route: Option<ModelRoute>,
}

/// The agent role; see [`ir::AgentRuntime`](crate::ir::AgentRuntime), whose rules its `dirs`
/// and `files` keep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent {
    pub role: RoleId,
    pub command: Command,
    /// In order of their paths.
    pub dirs: Vec<Dir>,
    pub files: Vec<File>,
    pub stdin: Option<String>,
    /// See [`ir::AgentRuntime::deadline_seconds`](crate::ir::AgentRuntime::deadline_seconds).
    pub deadline: Duration,
    /// See
    /// [`ir::AgentRuntime::finalize_grace_seconds`](crate::ir::AgentRuntime::finalize_grace_seconds).
    pub finalize_grace: Duration,
}

/// A judge role.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Judge {
    /// See [`ir::JudgeRuntime::command`](crate::ir::JudgeRuntime::command).
    pub command: Command,
    /// Each named once.
    pub slots: Vec<Slot>,
    /// See [`ir::JudgeRuntime::result`](crate::ir::JudgeRuntime::result).
    pub result: ResultSpec,
}

/// A judge's slot; see [`ir::Slot`](crate::ir::Slot).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub name: Name,
    /// At most [`MAX_OP_PAYLOAD_BYTES`](crate::sandbox::MAX_OP_PAYLOAD_BYTES), as parsing checks.
    pub(crate) max_bytes: NonZeroU64,
}

impl Slot {
    /// See [`ir::Slot::max_bytes`](crate::ir::Slot::max_bytes).
    pub fn max_bytes(&self) -> NonZeroU64 {
        self.max_bytes
    }

    /// The op payload limit of a session whose payloads the slot takes: its `max_bytes`.
    pub fn payload_limit(&self) -> OpPayloadLimit {
        OpPayloadLimit(self.max_bytes.get())
    }
}

/// A judge grant of the agent's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JudgeGrant {
    pub name: Name,
    /// The judge's slot that receives the payload.
    pub slot: SlotId,
    /// See [`ir::JudgeGrant::calls`](crate::ir::JudgeGrant::calls).
    pub calls: Option<NonZeroU64>,
    /// See [`ir::JudgeGrant::cumulative_seconds`](crate::ir::JudgeGrant::cumulative_seconds).
    pub cumulative_seconds: Option<NonZeroU64>,
    /// See
    /// [`ir::JudgeGrant::call_timeout_seconds`](crate::ir::JudgeGrant::call_timeout_seconds).
    pub call_timeout_seconds: NonZeroU64,
}

impl JudgeGrant {
    /// The judge, which no other judge grant names; see
    /// [`ir::JudgeGrant::role`](crate::ir::JudgeGrant::role).
    pub fn judge(&self) -> JudgeId {
        self.slot.judge
    }

    /// [`ir::JudgeGrant::cumulative_seconds`](crate::ir::JudgeGrant::cumulative_seconds), as a
    /// [`Duration`].
    pub fn cumulative(&self) -> Option<Duration> {
        self.cumulative_seconds
            .map(|seconds| Duration::from_secs(seconds.get()))
    }

    /// [`ir::JudgeGrant::call_timeout_seconds`](crate::ir::JudgeGrant::call_timeout_seconds), as
    /// a [`Duration`].
    pub fn call_timeout(&self) -> Duration {
        Duration::from_secs(self.call_timeout_seconds.get())
    }
}

/// A runner role; see [`ir::RunnerRuntime`](crate::ir::RunnerRuntime).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Runner {
    pub cwd: AbsolutePath,
    pub env: Env,
}

/// A lease grant of the agent's; see [`ir::LeaseGrant`](crate::ir::LeaseGrant).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseGrant {
    pub name: Name,
    /// The role that holds it: the agent.
    pub holder: RoleId,
    /// The runner, which no other lease grant names, and whose image is no judge's.
    pub runner: RunnerId,
    pub instances: NonZeroU32,
    pub instance_seconds: NonZeroU64,
    pub cumulative_seconds: Option<NonZeroU64>,
    pub commands: Option<NonZeroU64>,
    pub transfer_bytes: NonZeroU64,
}

impl LeaseGrant {
    /// [`ir::LeaseGrant::instance_seconds`](crate::ir::LeaseGrant::instance_seconds), as a
    /// [`Duration`].
    pub fn instance(&self) -> Duration {
        Duration::from_secs(self.instance_seconds.get())
    }

    /// [`ir::LeaseGrant::cumulative_seconds`](crate::ir::LeaseGrant::cumulative_seconds), as a
    /// [`Duration`].
    pub fn cumulative(&self) -> Option<Duration> {
        (self.cumulative_seconds).map(|seconds| Duration::from_secs(seconds.get()))
    }
}

/// The agent's submit grant; see [`ir::SubmitGrant`](crate::ir::SubmitGrant).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmitGrant {
    pub name: Name,
}

/// The parsed [`ir::Score`](crate::ir::Score): the slot of a judge no judge grant names, whose
/// `max_bytes` caps every submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Score {
    pub slot: SlotId,
    /// See [`ir::Score::timeout_seconds`](crate::ir::Score::timeout_seconds).
    pub timeout: Duration,
}
