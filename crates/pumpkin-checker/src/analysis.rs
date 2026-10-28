//! What the [`Graph`] implies: information paths, control, trust domains, authority, and the
//! nodes that run untrusted code, as [the model](crate#the-model) defines them.
//!
//! Every map here has an entry for every node of the graph, so lookups by node never miss.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::iter;

use pumpkin_schema::ir::SlotKind;

use crate::graph::{Ability, Graph, Node, Target};

/// For each node, a shortest information path to each target it reaches: the nodes the path
/// passes through, the node itself first and the target excluded.
pub(crate) type Paths = BTreeMap<Node, BTreeMap<Target, Vec<Node>>>;

/// For each node, the nodes it controls, itself included when it does.
pub(crate) type Controlled = BTreeMap<Node, BTreeSet<Node>>;

/// What a node reaches, controls and can use: its authority, Auth(R), in one phase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authority {
    pub node: Node,
    /// The targets the node has an information path to, other than itself.
    pub reaches: BTreeSet<Target>,
    /// The nodes the node controls, other than itself.
    pub controls: BTreeSet<Node>,
    /// Auth(R): what the node can use, itself or through the nodes it controls.
    pub uses: Vec<Use>,
}

/// An ability, and the node that holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Use {
    pub by: Node,
    pub ability: Ability,
}

/// A node that runs untrusted code: an untrusted principal's, or one that an untrusted
/// principal controls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UntrustedCode {
    pub node: Node,
    /// The untrusted principals' nodes whose code it runs, itself included when it is one.
    pub sources: BTreeSet<Node>,
}

/// A map from every node of the graph to the values `pairs` lists for it.
fn by_node<T>(graph: &Graph, pairs: impl Iterator<Item = (Node, T)>) -> BTreeMap<Node, Vec<T>> {
    let mut map: BTreeMap<Node, Vec<T>> = graph.nodes.keys().map(|&n| (n, Vec::new())).collect();
    for (node, value) in pairs {
        map.entry(node).or_default().push(value);
    }
    map
}

/// Each survivor's node before and after a transition.
fn survivals(graph: &Graph) -> impl Iterator<Item = (Node, Node)> {
    graph.transitions.iter().flat_map(|transition| {
        transition.survivors.iter().map(|&principal| {
            let node = |phase| Node { phase, principal };
            (node(transition.from), node(transition.to))
        })
    })
}

/// The information paths from every node, found breadth first.
pub(crate) fn paths(graph: &Graph) -> Paths {
    let flows = graph.flows.iter().map(|flow| (flow.from, flow.to));
    let survivals = survivals(graph).map(|(from, to)| (from, Target::Node(to)));
    let successors = by_node(graph, flows.chain(survivals));
    let mut paths = Paths::new();
    for &source in graph.nodes.keys() {
        let mut found: BTreeMap<Target, Vec<Node>> = BTreeMap::new();
        let mut queue = VecDeque::from([(source, vec![source])]);
        while let Some((node, path)) = queue.pop_front() {
            for &target in &successors[&node] {
                if found.contains_key(&target) {
                    continue;
                }
                found.insert(target, path.clone());
                if let Target::Node(next) = target {
                    let mut next_path = path.clone();
                    next_path.push(next);
                    queue.push_back((next, next_path));
                }
            }
        }
        paths.insert(source, found);
    }
    paths
}

/// The nodes every node controls.
pub(crate) fn controlled(graph: &Graph, paths: &Paths) -> Controlled {
    let code = graph
        .flows
        .iter()
        .filter_map(|flow| match (flow.kind, flow.to) {
            (SlotKind::Code, Target::Node(to)) => Some((flow.from, to)),
            (SlotKind::Data, Target::Node(_) | Target::Operator | Target::ModelProvider)
            | (SlotKind::Code, Target::Operator | Target::ModelProvider) => None,
        });
    let code = by_node(graph, code);
    let survives = by_node(graph, survivals(graph));
    let mut controlled = Controlled::new();
    for (&source, found) in paths {
        let reached = found.keys().filter_map(|&target| match target {
            Target::Node(node) => Some(node),
            Target::Operator | Target::ModelProvider => None,
        });
        let mut nodes = BTreeSet::new();
        let mut queue: VecDeque<Node> = iter::once(source)
            .chain(reached)
            .flat_map(|sender| code[&sender].iter().copied())
            .collect();
        while let Some(node) = queue.pop_front() {
            if nodes.insert(node) {
                queue.extend(&survives[&node]);
            }
        }
        controlled.insert(source, nodes);
    }
    controlled
}

/// The trust domains: each set of two or more nodes that control each other.
pub(crate) fn trust_domains(controlled: &Controlled) -> Vec<BTreeSet<Node>> {
    let domains: BTreeSet<BTreeSet<Node>> = controlled
        .iter()
        .map(|(node, nodes)| {
            let mutual = nodes
                .iter()
                .filter(|other| controlled[*other].contains(node));
            iter::once(node)
                .chain(mutual)
                .copied()
                .collect::<BTreeSet<_>>()
        })
        .filter(|domain| domain.len() > 1)
        .collect();
    domains.into_iter().collect()
}

/// Every node that runs untrusted code, with whose.
pub(crate) fn untrusted_code(graph: &Graph, controlled: &Controlled) -> Vec<UntrustedCode> {
    let untrusted = controlled
        .iter()
        .filter(|(node, _)| graph.untrusted.contains(&node.principal));
    let mut sources: BTreeMap<Node, BTreeSet<Node>> = BTreeMap::new();
    for (&source, nodes) in untrusted {
        for &node in iter::once(&source).chain(nodes) {
            sources.entry(node).or_default().insert(source);
        }
    }
    sources
        .into_iter()
        .map(|(node, sources)| UntrustedCode { node, sources })
        .collect()
}

/// The authority of every node.
pub(crate) fn authority(graph: &Graph, paths: &Paths, controlled: &Controlled) -> Vec<Authority> {
    let mut authority = Vec::new();
    for (&node, found) in paths {
        let controls: BTreeSet<Node> = (controlled[&node].iter().copied())
            .filter(|&other| other != node)
            .collect();
        let uses = iter::once(&node)
            .chain(&controls)
            .flat_map(|&by| {
                let abilities = graph.nodes[&by].iter();
                abilities.map(move |&ability| Use { by, ability })
            })
            .collect();
        let reaches = (found.keys().copied())
            .filter(|&target| target != Target::Node(node))
            .collect();
        authority.push(Authority {
            node,
            reaches,
            controls,
            uses,
        });
    }
    authority
}
