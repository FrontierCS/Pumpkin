//! A workflow parsed from its IR: references resolved to typed ids, every time given as a
//! [`Duration`], and the rules that span its parts checked.
//!
//! A [`Placed`] workflow has each of its roles in the smallest class of sandbox that fits it. A
//! [`Workflow`] can only be made by [`Workflow::parse`], and a [`Placed`] by
//! [`Workflow::place`], so code that acts on one need not check again what they established.
//! Each kind of part has its own id type, so that, for example, the role a [`JudgeGrant`]
//! judges with is known to be a [`Judge`]. Ids are only valid for the workflow that issued them.

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::time::Duration;

use serde::Serialize;

use crate::ir::{
    Command, Dir, File, Image, JsonPointer, ModelRoute, Name, Resources, ResultSpec, User,
    WorkflowName,
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
    judge_grants: Vec<JudgeGrant>,
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

/// A parsed workflow whose every role is placed in the smallest class of sandbox that fits it
/// ([`Workflow::place`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    workflow: Workflow,
    /// Each role's class's [`startup_seconds`](Class::startup_seconds).
    startups: ByRole<Duration>,
}

impl Placed {
    pub fn workflow(&self) -> &Workflow {
        &self.workflow
    }

    /// The longest a start of one of `role`'s sandboxes takes, as its class declares.
    pub fn startup(&self, role: RoleId) -> Duration {
        *self.startups.get(role)
    }
}

/// A class of sandbox that a provider creates, such as a size of machine, and what a run may
/// rely on of its sandboxes, which a role is placed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Class {
    /// The most a role whose sandboxes are of the class may ask for of each resource, so a role
    /// fits the class if it asks for no more.
    pub resources: Resources,
    /// The most seconds a sandbox's start takes, from the start of its create until its
    /// `sandboxd` has met the gateway and its session is set up.
    pub startup_seconds: NonZeroU64,
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
    pub needs: Resources,
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
