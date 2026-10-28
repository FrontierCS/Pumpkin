//! The assumptions a check rests on, each with its author. A check proves its properties only
//! as far as these hold.

use std::collections::{BTreeMap, BTreeSet};

use pumpkin_schema::ir::{Models, Name, SlotKind, Trust};
use pumpkin_schema::workflow::{JudgeCall, JudgeId, RoleId, Workflow};

use crate::analysis::UntrustedCode;
use crate::graph::{self, Cause, Graph, Node};

/// Something the check takes on trust.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Assumption {
    /// A judge declared trusted: its own code acts as its author intends.
    Trusted(JudgeId),
    /// A trusted judge parses this slot without running it. An untrusted judge's slots are
    /// all taken as code, so they need no assumption.
    DataSlot { judge: JudgeId, slot: Name },
    /// A role's image holds no final-mode data other than the assets it declares final-mode.
    Assets(RoleId),
    /// A role's model route is closed: one pinned upstream, no onward fetching, and no state
    /// shared across requests.
    ClosedRoute { role: RoleId, models: Models },
    /// A trusted judge's results to a call, computed in a node that runs untrusted code, rest
    /// on the task's own isolation inside the judge's sandbox: the untrusted code cannot forge
    /// them.
    TaskIsolation {
        call: JudgeCall,
        judge: Node,
        /// See [`UntrustedCode::sources`].
        sources: BTreeSet<Node>,
    },
    /// A call's results come from an untrusted judge's own code, which nothing inside its
    /// sandbox guards against: the workflow takes them as they are.
    UntrustedResults { call: JudgeCall, judge: Node },
}

/// Who vouches for an assumption.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Author {
    /// The workflow, named by its name; the IR does not say who wrote which part.
    Workflow,
    /// Pumpkin, whose gateway serves and closes model routes.
    Pumpkin,
}

impl Assumption {
    pub fn author(&self) -> Author {
        match self {
            Self::Trusted(_)
            | Self::DataSlot { .. }
            | Self::Assets(_)
            | Self::TaskIsolation { .. }
            | Self::UntrustedResults { .. } => Author::Workflow,
            Self::ClosedRoute { .. } => Author::Pumpkin,
        }
    }
}

/// The assumptions the workflow declares: its trusted judges and their data slots, its roles'
/// assets, and its model routes.
pub(crate) fn declared(workflow: &Workflow) -> Vec<Assumption> {
    let trusted: Vec<JudgeId> = workflow
        .judges()
        .filter(|(_, judge)| match judge.trust {
            Trust::Trusted => true,
            Trust::Untrusted => false,
        })
        .map(|(judge, _)| judge)
        .collect();
    let data_slots = trusted.iter().flat_map(|&judge| {
        let slots = workflow.judge(judge).slots.iter();
        let data = slots.filter(|slot| match slot.kind {
            SlotKind::Data => true,
            SlotKind::Code => false,
        });
        data.map(move |slot| Assumption::DataSlot {
            judge,
            slot: slot.name.clone(),
        })
    });
    let roles = graph::roles(workflow);
    let assets = roles.iter().map(|&role| Assumption::Assets(role));
    let routes = roles.iter().filter_map(|&role| {
        let route = workflow.sandbox(role).model_route.as_ref()?;
        let models = route.models.clone();
        Some(Assumption::ClosedRoute { role, models })
    });
    (trusted.iter().map(|&judge| Assumption::Trusted(judge)))
        .chain(data_slots)
        .chain(assets)
        .chain(routes)
        .collect()
}

/// What each call's results computed in a node that runs untrusted code rest on: the task's
/// isolation in a trusted judge, or nothing in an untrusted one.
pub(crate) fn task_isolation(graph: &Graph, untrusted_code: &[UntrustedCode]) -> Vec<Assumption> {
    let sources: BTreeMap<Node, &BTreeSet<Node>> = untrusted_code
        .iter()
        .map(|code| (code.node, &code.sources))
        .collect();
    let results = graph.flows.iter().filter_map(|flow| match flow.cause {
        Cause::Result(call) => Some((call, flow.from)),
        Cause::Payload(_)
        | Cause::Submission
        | Cause::Receipt
        | Cause::ModelRequest
        | Cause::ModelResponse
        | Cause::Outcome => None,
    });
    results
        .filter_map(|(call, judge)| {
            let sources = (*sources.get(&judge)?).clone();
            Some(if graph.untrusted.contains(&judge.principal) {
                Assumption::UntrustedResults { call, judge }
            } else {
                Assumption::TaskIsolation {
                    call,
                    judge,
                    sources,
                }
            })
        })
        .collect()
}
