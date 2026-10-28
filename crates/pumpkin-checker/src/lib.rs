//! Pumpkin's checker: proves properties of a workflow before it runs, and names every
//! assumption the proof rests on. A workflow that fails the check must never run:
//! [`Report::passed`] says whether it may, and the report's text is the reason to give.
//!
//! The checker is a pure library: no I/O, and no dependency but [`pumpkin_schema`].
//!
//! # The model
//!
//! [`check`] expands a parsed [`Workflow`]'s pattern into a time-expanded graph: a [`Node`] is
//! a principal's instances in a [`Phase`] in which it is live, a [`Flow`] carries a value from
//! a node to a [`Target`] within a phase, and a [`Transition`] carries survivors from one phase
//! to the next. The root, the workflow's pattern, is trusted; the agent is untrusted, and so is
//! every judge declared untrusted. A value lands as data only in the root, in the targets
//! outside the phases, and in a trusted judge's data slot; anywhere else as [`SlotKind::Code`].
//!
//! An **information path** follows flows within a phase and survivors from a phase to a next,
//! so it only moves forward in time. A node **controls** another when it is, or has an
//! information path to, the sender of a code flow into the other, or into an earlier node of
//! the same principal that the other survives from. Treating any information path into a
//! sender as influence on all it sends is a safe over-approximation: a value forwarded out of a
//! data slot into a code slot still gives its original author control. Control so defined is
//! transitive.
//!
//! From the graph the checker derives each node's [`Authority`], the nodes that run
//! [`UntrustedCode`], and the trust domains, sets of nodes that control each other.
//!
//! # Rules and assumptions
//!
//! A workflow passes when it breaks no rule; see [`Violation`]. These are the per-mode rule's
//! two halves: final-mode assets only on final-mode judges, in what they declare and in the
//! images they run, and no information path from a final-mode judge to an untrusted principal
//! live in the same or a later phase, so nothing a final-mode judge produces reaches a working
//! agent.
//!
//! The proof holds only as far as each [`Assumption`] does: the trusted judges, their data
//! slots, each role's asset declarations, the closure of model routes, and, for every score
//! computed in a node that runs untrusted code, the task's own isolation, or an untrusted
//! judge's own code.
//!
//! [`SlotKind::Code`]: pumpkin_schema::ir::SlotKind::Code

use pumpkin_schema::workflow::Workflow;

mod analysis;
mod assumptions;
mod graph;
mod report;
mod rules;

pub use analysis::{Authority, UntrustedCode, Use};
pub use assumptions::{Assumption, Author};
pub use graph::{Ability, Cause, Flow, Node, Phase, Principal, Target, Transition};
pub use report::Report;
pub use rules::Violation;

/// Checks a workflow.
pub fn check(workflow: &Workflow) -> Report<'_> {
    let graph = graph::Graph::new(workflow);
    let paths = analysis::paths(&graph);
    let controlled = analysis::controlled(&graph, &paths);
    let untrusted_code = analysis::untrusted_code(&graph, &controlled);
    let violations = rules::final_mode_assets(workflow)
        .into_iter()
        .chain(rules::final_mode_paths(&graph, &paths))
        .collect();
    let assumptions = assumptions::declared(workflow)
        .into_iter()
        .chain(assumptions::task_isolation(&graph, &untrusted_code))
        .collect();
    Report {
        workflow,
        authority: analysis::authority(&graph, &paths, &controlled),
        trust_domains: analysis::trust_domains(&controlled),
        untrusted_code,
        violations,
        assumptions,
        flows: graph.flows,
        transitions: graph.transitions,
    }
}
