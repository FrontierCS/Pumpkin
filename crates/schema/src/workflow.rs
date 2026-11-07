//! A workflow parsed from its IR: references resolved to typed ids, every time given as a
//! [`Duration`], and the rules that span its parts checked.
//!
//! A [`Workflow`] can only be made by [`Workflow::parse`], so code that acts on one need not
//! check again what parsing established. Each kind of part has its own id type, so that, for
//! example, the role a [`JudgeGrant`] judges with is known to be a [`Judge`]. Ids are only
//! valid for the workflow that issued them.

use std::num::NonZeroU64;
use std::time::Duration;

use crate::ir::{
    Command, Dir, File, Image, ModelRoute, Name, Resources, ResultSpec, Slot, User, WorkflowName,
};

mod parse;

pub use parse::{ParseError, ParseErrors};

/// A parsed workflow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workflow {
    ir: crate::ir::Workflow,
    name: WorkflowName,
    agent: Agent,
    judges: Vec<Judge>,
    judge_grants: Vec<JudgeGrant>,
    submit_grant: SubmitGrant,
    main: Main,
}

impl Workflow {
    /// The IR that this workflow was parsed from.
    pub fn ir(&self) -> &crate::ir::Workflow {
        &self.ir
    }

    pub fn name(&self) -> &WorkflowName {
        &self.name
    }

    /// The workflow's one agent.
    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    pub fn judges(&self) -> impl Iterator<Item = (JudgeId, &Judge)> {
        self.judges
            .iter()
            .enumerate()
            .map(|(index, judge)| (JudgeId(index), judge))
    }

    pub fn judge(&self, id: JudgeId) -> &Judge {
        &self.judges[id.0]
    }

    /// The sandbox of any role.
    pub fn sandbox(&self, role: RoleId) -> &Sandbox {
        match role {
            RoleId::Agent => &self.agent.sandbox,
            RoleId::Judge(judge) => &self.judge(judge).sandbox,
        }
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

    /// The slot a judge call's payload lands in.
    pub fn call_slot(&self, call: JudgeCall) -> SlotId {
        match (call, &self.main) {
            (JudgeCall::Grant(grant), _) => self.judge_grant(grant).slot,
            (JudgeCall::Score, Main::SoloThenJudge(pattern)) => pattern.score.slot,
        }
    }

    pub fn main(&self) -> &Main {
        &self.main
    }
}

/// A judge role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JudgeId(usize);

/// A role of either kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RoleId {
    /// The workflow's one agent.
    Agent,
    Judge(JudgeId),
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

/// What a judge call is made under: one of the agent's judge grants, or the pattern's
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
    pub resources: Resources,
    /// Only the agent's may be set.
    pub model_route: Option<ModelRoute>,
}

/// The agent role; see [`ir::AgentRuntime`](crate::ir::AgentRuntime), whose rules its `dirs`
/// and `files` keep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent {
    pub sandbox: Sandbox,
    pub command: Command,
    /// In order of their paths.
    pub dirs: Vec<Dir>,
    pub files: Vec<File>,
    pub stdin: Option<String>,
}

/// A judge role.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Judge {
    pub sandbox: Sandbox,
    /// See [`ir::JudgeRuntime::command`](crate::ir::JudgeRuntime::command).
    pub command: Command,
    /// Each named once.
    pub slots: Vec<Slot>,
    /// See [`ir::JudgeRuntime::result`](crate::ir::JudgeRuntime::result).
    pub result: ResultSpec,
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

/// The pattern that drives a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Main {
    SoloThenJudge(SoloThenJudge),
}

/// The parsed [`ir::SoloThenJudge`](crate::ir::SoloThenJudge). Its grants are the
/// [workflow's](Workflow::judge_grants).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoloThenJudge {
    pub score: Score,
    /// See [`ir::SoloThenJudge::deadline_seconds`](crate::ir::SoloThenJudge::deadline_seconds).
    pub deadline: Duration,
    /// See
    /// [`ir::SoloThenJudge::finalize_grace_seconds`](crate::ir::SoloThenJudge::finalize_grace_seconds).
    pub finalize_grace: Duration,
}

/// The parsed [`ir::Score`](crate::ir::Score): the slot of a judge no judge grant names, whose
/// `max_bytes` caps every submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Score {
    pub slot: SlotId,
    /// See [`ir::Score::timeout_seconds`](crate::ir::Score::timeout_seconds).
    pub timeout: Duration,
}
