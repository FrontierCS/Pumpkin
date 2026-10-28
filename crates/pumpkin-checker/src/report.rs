//! The result of a check, and how it is printed.

use std::collections::BTreeSet;
use std::fmt;

use pumpkin_schema::workflow::Workflow;

use crate::analysis::{Authority, UntrustedCode};
use crate::assumptions::Assumption;
use crate::graph::{Flow, Node, Transition};
use crate::rules::Violation;

mod render;

/// What a check found about a workflow: the rules it breaks, the assumptions the check rests
/// on, and the model behind both. Only [`check`](crate::check) makes one.
///
/// Its [`Display`](fmt::Display) form is what the operator reads. It names principals, phases
/// and grants as the workflow does, and is the same for the same workflow.
#[derive(Clone, Debug)]
pub struct Report<'w> {
    pub(crate) workflow: &'w Workflow,
    pub(crate) flows: Vec<Flow>,
    pub(crate) transitions: Vec<Transition>,
    pub(crate) authority: Vec<Authority>,
    pub(crate) trust_domains: Vec<BTreeSet<Node>>,
    pub(crate) untrusted_code: Vec<UntrustedCode>,
    pub(crate) violations: Vec<Violation>,
    pub(crate) assumptions: Vec<Assumption>,
}

impl<'w> Report<'w> {
    /// The workflow checked.
    pub fn workflow(&self) -> &'w Workflow {
        self.workflow
    }

    /// Whether the workflow breaks no rule, and so may run.
    pub fn passed(&self) -> bool {
        self.violations.is_empty()
    }

    /// The rules the workflow breaks.
    pub fn violations(&self) -> &[Violation] {
        &self.violations
    }

    /// Every assumption the check rests on.
    pub fn assumptions(&self) -> &[Assumption] {
        &self.assumptions
    }

    /// The values that flow between nodes, in phase order.
    pub fn flows(&self) -> &[Flow] {
        &self.flows
    }

    /// The moves between phases, and who survives them.
    pub fn transitions(&self) -> &[Transition] {
        &self.transitions
    }

    /// The authority of every node, in node order.
    pub fn authority(&self) -> &[Authority] {
        &self.authority
    }

    /// Each set of two or more nodes that control each other.
    pub fn trust_domains(&self) -> &[BTreeSet<Node>] {
        &self.trust_domains
    }

    /// Every node that runs untrusted code, in node order.
    pub fn untrusted_code(&self) -> &[UntrustedCode] {
        &self.untrusted_code
    }
}

impl fmt::Display for Report<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&render::lines(self).join("\n"))
    }
}
