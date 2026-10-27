//! A workflow parsed from its IR: references resolved to typed ids, and the rules that span
//! its parts checked.
//!
//! A [`Workflow`] can only be made by [`Workflow::parse`], so code that acts on one need not
//! check again what parsing established. Each kind of part has its own id type, so that, for
//! example, the role a [`JudgeGrant`] judges with is known to be a [`Judge`]. Ids are only
//! valid for the workflow that issued them.

use std::collections::BTreeSet;
use std::num::{NonZeroU32, NonZeroU64};

use crate::ir::{
    Asset, Command, Image, Mode, ModelRoute, Name, Resources, Slot, Trust, User, WorkflowName,
};

mod parse;

pub use parse::{ParseError, ParseErrors};

/// A parsed workflow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workflow {
    name: WorkflowName,
    agents: Vec<Agent>,
    judges: Vec<Judge>,
    judge_grants: Vec<JudgeGrant>,
    submit_grants: Vec<SubmitGrant>,
    phases: Vec<Phase>,
    main: Main,
}

impl Workflow {
    pub fn name(&self) -> &WorkflowName {
        &self.name
    }

    pub fn agents(&self) -> impl Iterator<Item = (AgentId, &Agent)> {
        self.agents
            .iter()
            .enumerate()
            .map(|(index, agent)| (AgentId(index), agent))
    }

    pub fn agent(&self, id: AgentId) -> &Agent {
        &self.agents[id.0]
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
            RoleId::Agent(agent) => &self.agent(agent).sandbox,
            RoleId::Judge(judge) => &self.judge(judge).sandbox,
        }
    }

    /// The trust level of any role: agents are untrusted.
    pub fn trust(&self, role: RoleId) -> Trust {
        match role {
            RoleId::Agent(_) => Trust::Untrusted,
            RoleId::Judge(judge) => self.judge(judge).trust,
        }
    }

    pub fn slot(&self, id: SlotId) -> &Slot {
        &self.judge(id.judge).slots[id.index]
    }

    pub fn judge_grants(&self) -> impl Iterator<Item = (JudgeGrantId, &JudgeGrant)> {
        self.judge_grants
            .iter()
            .enumerate()
            .map(|(index, grant)| (JudgeGrantId(index), grant))
    }

    pub fn judge_grant(&self, id: JudgeGrantId) -> &JudgeGrant {
        &self.judge_grants[id.0]
    }

    pub fn submit_grants(&self) -> impl Iterator<Item = (SubmitGrantId, &SubmitGrant)> {
        self.submit_grants
            .iter()
            .enumerate()
            .map(|(index, grant)| (SubmitGrantId(index), grant))
    }

    pub fn submit_grant(&self, id: SubmitGrantId) -> &SubmitGrant {
        &self.submit_grants[id.0]
    }

    /// The phases, in the IR's order.
    pub fn phases(&self) -> impl Iterator<Item = (PhaseId, &Phase)> {
        self.phases
            .iter()
            .enumerate()
            .map(|(index, phase)| (PhaseId(index), phase))
    }

    pub fn phase(&self, id: PhaseId) -> &Phase {
        &self.phases[id.0]
    }

    /// The phase a run starts in.
    pub fn initial_phase(&self) -> PhaseId {
        PhaseId(0)
    }

    pub fn main(&self) -> &Main {
        &self.main
    }
}

/// An agent role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentId(usize);

/// A judge role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JudgeId(usize);

/// A role of either kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RoleId {
    Agent(AgentId),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubmitGrantId(usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhaseId(usize);

/// What every role has: the sandbox its instances run in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sandbox {
    pub name: Name,
    pub image: Image,
    pub user: User,
    pub resources: Resources,
    pub model_route: Option<ModelRoute>,
    /// Each named once.
    pub assets: Vec<Asset>,
}

/// An agent role; see [`ir::Runtime::Agent`](crate::ir::Runtime::Agent).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent {
    pub sandbox: Sandbox,
    /// See [`ir::AgentRuntime::command`](crate::ir::AgentRuntime::command).
    pub command: Command,
}

/// A judge role.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Judge {
    pub sandbox: Sandbox,
    /// See [`ir::JudgeRuntime::command`](crate::ir::JudgeRuntime::command).
    pub command: Command,
    pub mode: Mode,
    pub trust: Trust,
    /// Each named once.
    pub slots: Vec<Slot>,
}

/// The holder of a grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Holder {
    /// The workflow's pattern.
    Root,
    /// Every instance of a role.
    Role(RoleId),
}

/// A grant of the judge op. Its holder and its judge are live in every phase it applies in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JudgeGrant {
    pub name: Name,
    pub holder: Holder,
    /// Never empty.
    pub phases: BTreeSet<PhaseId>,
    /// The judge's slot that receives the payload.
    pub slot: SlotId,
    /// See [`ir::JudgeOp::calls`](crate::ir::JudgeOp::calls).
    pub calls: Option<NonZeroU64>,
    /// See [`ir::JudgeOp::cumulative_seconds`](crate::ir::JudgeOp::cumulative_seconds).
    pub cumulative_seconds: Option<NonZeroU64>,
    /// See [`ir::JudgeOp::concurrency`](crate::ir::JudgeOp::concurrency).
    pub concurrency: NonZeroU32,
    /// See [`ir::JudgeOp::call_timeout_seconds`](crate::ir::JudgeOp::call_timeout_seconds).
    pub call_timeout_seconds: Option<NonZeroU64>,
}

impl JudgeGrant {
    pub fn judge(&self) -> JudgeId {
        self.slot.judge
    }
}

/// A grant of the submit op, which only a role holds. Its holder is live in every phase it
/// applies in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmitGrant {
    pub name: Name,
    pub holder: RoleId,
    /// Never empty.
    pub phases: BTreeSet<PhaseId>,
    /// See [`ir::SubmitOp::max_bytes`](crate::ir::SubmitOp::max_bytes).
    pub max_bytes: NonZeroU64,
}

/// A phase of a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Phase {
    pub name: Name,
    pub live: BTreeSet<RoleId>,
    /// Never this phase.
    pub next: BTreeSet<PhaseId>,
}

impl Phase {
    pub fn is_live(&self, role: RoleId) -> bool {
        self.live.contains(&role)
    }
}

/// The pattern that drives a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Main {
    SoloThenJudge(SoloThenJudge),
}

/// The parsed [`ir::SoloThenJudge`](crate::ir::SoloThenJudge).
///
/// The work phase is the initial phase and is followed by the final phase. The agent is live
/// in the work phase and not in the final phase. The agent holds the submit grant, which
/// applies in the work phase. The root holds the score grant, which applies in the final
/// phase, judges with a final-mode judge, and has a slot that fits any submission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoloThenJudge {
    pub agent: AgentId,
    pub submit: SubmitGrantId,
    pub score: JudgeGrantId,
    pub work_phase: PhaseId,
    pub final_phase: PhaseId,
    /// See [`ir::SoloThenJudge::deadline_seconds`](crate::ir::SoloThenJudge::deadline_seconds).
    pub deadline_seconds: NonZeroU64,
}
