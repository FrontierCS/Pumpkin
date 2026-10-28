//! The rules a workflow must keep to run: the per-mode rule, which keeps final-mode data away
//! from untrusted principals.

use std::collections::BTreeMap;
use std::iter;

use pumpkin_schema::ir::{Asset, AssetMode, Mode, Name};
use pumpkin_schema::workflow::{RoleId, Workflow};

use crate::analysis::Paths;
use crate::graph::{self, Graph, Node, Target};

/// A rule a workflow breaks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Violation {
    /// A role other than a final-mode judge declares a final-mode asset.
    FinalModeAsset { role: RoleId, asset: Name },
    /// A role other than a final-mode judge runs the image of `holder`, which declares a
    /// final-mode asset, so its image holds that asset too.
    FinalModeImage { role: RoleId, holder: RoleId },
    /// A final-mode judge has an information path to an untrusted principal live in the same
    /// or a later phase. The path runs from the judge's node `from` through the nodes
    /// `through` to the principal's node `to`.
    FinalModePath {
        from: Node,
        through: Vec<Node>,
        to: Node,
    },
}

/// The roles that final-mode data must stay out of: the agent and train-mode judges.
fn constrained(workflow: &Workflow) -> Vec<RoleId> {
    let train_judges = workflow
        .judges()
        .filter(|(_, judge)| match judge.mode {
            Mode::Train => true,
            Mode::Final => false,
        })
        .map(|(judge, _)| RoleId::Judge(judge));
    iter::once(RoleId::Agent).chain(train_judges).collect()
}

/// The final-mode assets of a role.
fn final_mode(workflow: &Workflow, role: RoleId) -> impl Iterator<Item = &Asset> {
    let assets = workflow.sandbox(role).assets.iter();
    assets.filter(|asset| match asset.mode {
        AssetMode::Final => true,
        AssetMode::Any => false,
    })
}

/// The final-mode assets that roles other than final-mode judges hold: those they declare, and
/// those in images they share with a role that declares one. Images are the same when they
/// are pinned by the same digest.
pub(crate) fn final_mode_assets(workflow: &Workflow) -> Vec<Violation> {
    let mut holders: BTreeMap<&str, Vec<RoleId>> = BTreeMap::new();
    for role in graph::roles(workflow) {
        if final_mode(workflow, role).next().is_some() {
            let image = workflow.sandbox(role).image.digest();
            holders.entry(image).or_default().push(role);
        }
    }
    let mut violations = Vec::new();
    for role in constrained(workflow) {
        violations.extend(
            final_mode(workflow, role).map(|asset| Violation::FinalModeAsset {
                role,
                asset: asset.name.clone(),
            }),
        );
        let image = workflow.sandbox(role).image.digest();
        let sharing = holders.get(image).into_iter().flatten();
        violations.extend(
            (sharing.filter(|&&holder| holder != role))
                .map(|&holder| Violation::FinalModeImage { role, holder }),
        );
    }
    violations
}

/// The information paths from final-mode judges to untrusted principals. The graph is
/// time-expanded, so every node a path reaches is live in the same phase as the judge or a
/// later one. A path from a judge to itself is no violation: an untrusted final-mode judge
/// already holds its own data.
pub(crate) fn final_mode_paths(graph: &Graph, paths: &Paths) -> Vec<Violation> {
    let judges = paths
        .iter()
        .filter(|(node, _)| graph.final_mode.contains(&node.principal));
    let mut violations = Vec::new();
    for (&from, found) in judges {
        for (&target, path) in found {
            if let Target::Node(to) = target
                && to.principal != from.principal
                && graph.untrusted.contains(&to.principal)
            {
                violations.push(Violation::FinalModePath {
                    from,
                    through: path.iter().skip(1).copied().collect(),
                    to,
                });
            }
        }
    }
    violations
}
