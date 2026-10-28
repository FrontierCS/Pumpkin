//! The checker's boundary: a parsed [`Workflow`] becomes a time-expanded [`Graph`], built
//! once, which the analysis takes in its place.
//!
//! The graph expands the workflow's pattern, `solo_then_judge`, into its phases: in
//! [`Phase::Work`], the agent and every judge its grants name; in [`Phase::Final`], the judge
//! that scores the selection. The root is live in both, and alone survives from one to the
//! other: the agent's sandbox and each of its calls are gone before judging starts. Every node
//! is a principal live in a phase, and every flow joins nodes of one phase, so the analysis
//! never asks whether a principal is live or a flow is timely.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::iter;

use pumpkin_schema::ir::{Mode, SlotKind, Trust};
use pumpkin_schema::workflow::{JudgeCall, Main, RoleId, Workflow};

/// A phase of a run, as the pattern fixes them, in the order a run moves through them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Phase {
    /// The agent works, and calls the judges its grants name.
    Work,
    /// The score's judge judges the selected submission.
    Final,
}

impl Phase {
    /// Every phase, in order.
    pub(crate) const ALL: [Self; 2] = [Self::Work, Self::Final];

    /// The phase's name in the report.
    pub fn name(self) -> &'static str {
        match self {
            Self::Work => "work",
            Self::Final => "final",
        }
    }
}

/// The root, the workflow's pattern, or a role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Principal {
    Root,
    /// Every instance of a role.
    Role(RoleId),
}

/// The instances of a principal, the root or a role, in one phase in which it is live: a node
/// of the time-expanded graph.
///
/// All instances of a role are one node, which over-approximates what any one of them can do.
/// The root is live in every phase. Nodes are ordered by phase, then [`RoleId`]'s order (the
/// agent, then the judges in the workflow's order), then the root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Node {
    pub phase: Phase,
    pub principal: Principal,
}

impl Node {
    fn order(self) -> (Phase, bool, Principal) {
        let is_root = match self.principal {
            Principal::Root => true,
            Principal::Role(_) => false,
        };
        (self.phase, is_root, self.principal)
    }
}

impl Ord for Node {
    fn cmp(&self, other: &Self) -> Ordering {
        self.order().cmp(&other.order())
    }
}

impl PartialOrd for Node {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Where information can go: a node, or a principal outside the run's phases.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Target {
    Node(Node),
    /// Receives the outcome. Trusted, and nothing flows from it back into a run.
    Operator,
    /// Serves model routes. Each route is closed, so the provider passes nothing from one
    /// request to another, and its responses carry nothing the requester didn't send; see
    /// [`Cause::ModelResponse`].
    ModelProvider,
}

/// A value that one node sends to a target within a phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Flow {
    pub from: Node,
    pub to: Target,
    /// How the value lands. A code flow gives control of its receiver; see
    /// [the model](crate#the-model).
    pub kind: SlotKind,
    pub cause: Cause,
}

/// Why a value flows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// A judge call's payload, from its caller into the judge's slot: the agent's, under one
    /// of its grants, or the root's, scoring the selection.
    Payload(JudgeCall),
    /// A judge call's results, from the judge back to its caller.
    Result(JudgeCall),
    /// The agent's submissions, to the root, which keeps them.
    Submission,
    /// What the root returns to the agent's submissions.
    Receipt,
    /// Requests over a role's model route.
    ModelRequest,
    /// The responses to those requests, which land in the role as code, the worst case. The
    /// route is closed, so a response carries only what its requester sent, and the flow runs
    /// from the node to itself: whatever reaches the node controls it.
    ModelResponse,
    /// The pattern's outcome, from the root to the operator.
    Outcome,
}

/// A move from one phase to the next. Its survivors carry what they hold, and whoever
/// controls them, into the next phase; every other instance is destroyed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transition {
    pub from: Phase,
    pub to: Phase,
    /// The root alone: the agent's sandbox and every call it made are gone before the final
    /// phase starts.
    pub survivors: BTreeSet<Principal>,
}

/// Something a node can use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Ability {
    /// A judge call: the agent's, under one of its grants, or the root's score.
    Judge(JudgeCall),
    /// The agent's submit grant.
    Submit,
    /// The model route of the node's role.
    ModelRoute,
}

/// A workflow as the analysis sees it.
///
/// Every node that a flow or transition names is in `nodes`: each phase holds the root and
/// the roles live in it, and the flows of a phase join those alone.
pub(crate) struct Graph {
    /// Every node, with what it can use itself: the agent's grants in the work phase, the
    /// score in the final phase, and its role's model route.
    pub(crate) nodes: BTreeMap<Node, Vec<Ability>>,
    /// In phase order.
    pub(crate) flows: Vec<Flow>,
    pub(crate) transitions: Vec<Transition>,
    /// The agent, and every judge declared untrusted.
    pub(crate) untrusted: BTreeSet<Principal>,
    /// Every final-mode judge.
    pub(crate) final_mode: BTreeSet<Principal>,
}

impl Graph {
    pub(crate) fn new(workflow: &Workflow) -> Self {
        let live = live(workflow);
        let mut flows: Vec<Flow> = judge_flows(workflow)
            .into_iter()
            .chain(submit_flows())
            .chain(model_flows(workflow, &live))
            .chain(iter::once(outcome()))
            .collect();
        flows.sort_by_key(|flow| flow.from.phase);
        let roles = roles(workflow);
        Self {
            nodes: nodes(workflow, &live),
            flows,
            transitions: vec![Transition {
                from: Phase::Work,
                to: Phase::Final,
                survivors: BTreeSet::from([Principal::Root]),
            }],
            untrusted: principals(&roles, |role| match workflow.trust(role) {
                Trust::Untrusted => true,
                Trust::Trusted => false,
            }),
            final_mode: principals(&roles, |role| match role {
                RoleId::Agent => false,
                RoleId::Judge(judge) => match workflow.judge(judge).mode {
                    Mode::Final => true,
                    Mode::Train => false,
                },
            }),
        }
    }
}

/// Every role: the agent, then the judges in the workflow's order.
pub(crate) fn roles(workflow: &Workflow) -> Vec<RoleId> {
    let judges = workflow.judges().map(|(judge, _)| RoleId::Judge(judge));
    iter::once(RoleId::Agent).chain(judges).collect()
}

fn principals(roles: &[RoleId], include: impl Fn(RoleId) -> bool) -> BTreeSet<Principal> {
    roles
        .iter()
        .copied()
        .filter(|&role| include(role))
        .map(Principal::Role)
        .collect()
}

/// The roles live in each phase: in the work phase, the agent and the judges its grants
/// name; in the final phase, the score's judge.
pub(crate) fn live(workflow: &Workflow) -> BTreeMap<Phase, BTreeSet<RoleId>> {
    let Main::SoloThenJudge(pattern) = workflow.main();
    let judges = workflow
        .judge_grants()
        .map(|(_, grant)| RoleId::Judge(grant.judge()));
    let work = iter::once(RoleId::Agent).chain(judges).collect();
    let final_ = BTreeSet::from([RoleId::Judge(pattern.score.slot.judge())]);
    BTreeMap::from([(Phase::Work, work), (Phase::Final, final_)])
}

/// The node of `principal` in `phase`.
fn node(phase: Phase, principal: Principal) -> Node {
    Node { phase, principal }
}

/// Every node, with the grants, score and model route it can use.
fn nodes(
    workflow: &Workflow,
    live: &BTreeMap<Phase, BTreeSet<RoleId>>,
) -> BTreeMap<Node, Vec<Ability>> {
    let mut nodes: BTreeMap<Node, Vec<Ability>> = BTreeMap::new();
    for (&phase, roles) in live {
        let roles = roles.iter().copied().map(Principal::Role);
        for principal in iter::once(Principal::Root).chain(roles) {
            nodes.insert(node(phase, principal), Vec::new());
        }
    }
    let agent = node(Phase::Work, Principal::Role(RoleId::Agent));
    let grants = (workflow.judge_grants())
        .map(|(id, _)| (agent, Ability::Judge(JudgeCall::Grant(id))))
        .chain([(agent, Ability::Submit)]);
    let score = (
        node(Phase::Final, Principal::Root),
        Ability::Judge(JudgeCall::Score),
    );
    let model = (model_routes(workflow, live).into_iter()).map(|node| (node, Ability::ModelRoute));
    for (node, ability) in grants.chain([score]).chain(model) {
        nodes.entry(node).or_default().push(ability);
    }
    nodes
}

/// The nodes whose role has a model route.
fn model_routes(workflow: &Workflow, live: &BTreeMap<Phase, BTreeSet<RoleId>>) -> Vec<Node> {
    let mut nodes = Vec::new();
    for (&phase, roles) in live {
        let routed = roles
            .iter()
            .filter(|&&role| workflow.sandbox(role).model_route.is_some());
        nodes.extend(routed.map(|&role| node(phase, Principal::Role(role))));
    }
    nodes
}

/// How a value returned to a principal lands. The root parses what it receives. A role runs
/// it: the agent has only code inputs, and the IR names no slot in which a judge receives what
/// its calls return, so the checker assumes the worst.
fn returned(principal: Principal) -> SlotKind {
    match principal {
        Principal::Root => SlotKind::Data,
        Principal::Role(_) => SlotKind::Code,
    }
}

/// How a payload lands in a judge's slot. Only a trusted judge's code can be relied on to parse
/// a data slot without running it, so every slot of an untrusted judge is code.
fn landing(trust: Trust, slot: SlotKind) -> SlotKind {
    match trust {
        Trust::Trusted => slot,
        Trust::Untrusted => SlotKind::Code,
    }
}

/// Each judge call's payloads into its judge's slot, and results back to its caller: the
/// agent's grants in the work phase, and the root's score in the final phase.
fn judge_flows(workflow: &Workflow) -> Vec<Flow> {
    let agent = node(Phase::Work, Principal::Role(RoleId::Agent));
    let root = node(Phase::Final, Principal::Root);
    let grants = (workflow.judge_grants()).map(|(id, _)| (agent, JudgeCall::Grant(id)));
    let mut flows = Vec::new();
    for (caller, call) in grants.chain([(root, JudgeCall::Score)]) {
        let slot = workflow.call_slot(call);
        let kind = landing(workflow.judge(slot.judge()).trust, workflow.slot(slot).kind);
        let judge = node(caller.phase, Principal::Role(RoleId::Judge(slot.judge())));
        let (payload, result) = (Cause::Payload(call), Cause::Result(call));
        flows.extend(exchange(caller, judge, kind, payload, result));
    }
    flows
}

/// The agent's submissions to the root, and the root's receipts back to it.
fn submit_flows() -> [Flow; 2] {
    let agent = node(Phase::Work, Principal::Role(RoleId::Agent));
    let root = node(Phase::Work, Principal::Root);
    let (submission, receipt) = (Cause::Submission, Cause::Receipt);
    exchange(agent, root, SlotKind::Data, submission, receipt)
}

/// A call from `holder`, the agent or the root, to `callee`, and its return.
fn exchange(holder: Node, callee: Node, kind: SlotKind, call: Cause, reply: Cause) -> [Flow; 2] {
    [
        Flow {
            from: holder,
            to: Target::Node(callee),
            kind,
            cause: call,
        },
        Flow {
            from: callee,
            to: Target::Node(holder),
            kind: returned(holder.principal),
            cause: reply,
        },
    ]
}

/// Each model route's requests to the model provider, and its responses.
fn model_flows(
    workflow: &Workflow,
    live: &BTreeMap<Phase, BTreeSet<RoleId>>,
) -> impl Iterator<Item = Flow> {
    model_routes(workflow, live).into_iter().flat_map(|node| {
        let request = Flow {
            from: node,
            to: Target::ModelProvider,
            kind: SlotKind::Data,
            cause: Cause::ModelRequest,
        };
        let response = Flow {
            from: node,
            to: Target::Node(node),
            kind: SlotKind::Code,
            cause: Cause::ModelResponse,
        };
        [request, response]
    })
}

/// The outcome, which the root sends to the operator once the pattern ends.
fn outcome() -> Flow {
    Flow {
        from: node(Phase::Final, Principal::Root),
        to: Target::Operator,
        kind: SlotKind::Data,
        cause: Cause::Outcome,
    }
}
