//! The report's text: one section per part of the check, most important first.

use std::collections::BTreeSet;
use std::iter;

use pumpkin_schema::ir::{AssetMode, Mode, SlotKind, Trust};
use pumpkin_schema::workflow::{JudgeCall, Main, RoleId, Workflow};

use super::Report;
use crate::analysis::{Authority, UntrustedCode};
use crate::assumptions::{Assumption, Author};
use crate::graph::{self, Ability, Cause, Node, Phase, Principal, Target};
use crate::rules::Violation;

/// The report's lines, without line breaks.
pub(super) fn lines(report: &Report<'_>) -> Vec<String> {
    let names = Names(report.workflow);
    let verdict = if report.passed() { "passes" } else { "fails" };
    let sections = [
        vec![format!("{}: {verdict}", report.workflow.name())],
        rules(&names, &report.violations),
        assumptions(&names, &report.assumptions),
        principals(&names),
        flows(&names, report),
        authority(&names, &report.authority),
        untrusted_code(&names, &report.untrusted_code),
        trust_domains(&names, &report.trust_domains),
    ];
    sections.concat()
}

/// Looks up the names the workflow gives its parts.
struct Names<'w>(&'w Workflow);

impl Names<'_> {
    fn workflow(&self) -> &Workflow {
        self.0
    }

    fn principal(&self, principal: Principal) -> &str {
        match principal {
            Principal::Root => "root",
            Principal::Role(role) => self.role(role),
        }
    }

    fn role(&self, role: RoleId) -> &str {
        self.0.sandbox(role).name.as_str()
    }

    fn node(&self, node: Node) -> String {
        let Node { phase, principal } = node;
        format!("{}@{}", self.principal(principal), phase.name())
    }

    /// A judge call by its grant's name, or as `score`, the root's.
    fn call(&self, call: JudgeCall) -> &str {
        match call {
            JudgeCall::Grant(grant) => self.0.judge_grant(grant).name.as_str(),
            JudgeCall::Score => "score",
        }
    }

    fn nodes<'n>(&self, nodes: impl IntoIterator<Item = &'n Node>) -> String {
        list(nodes.into_iter().map(|&node| self.node(node)))
    }

    fn target(&self, target: Target) -> String {
        match target {
            Target::Node(node) => self.node(node),
            Target::Operator => "operator".to_owned(),
            Target::ModelProvider => "model provider".to_owned(),
        }
    }

    fn ability(&self, ability: Ability) -> &str {
        match ability {
            Ability::Judge(call) => self.call(call),
            Ability::Submit => self.0.submit_grant().name.as_str(),
            Ability::ModelRoute => "model route",
        }
    }

    fn cause(&self, cause: Cause) -> String {
        let submit = &self.0.submit_grant().name;
        match cause {
            Cause::Payload(call) => {
                let slot = &self.0.slot(self.0.call_slot(call)).name;
                format!("{}'s payload, into slot {slot}", self.call(call))
            }
            Cause::Result(call) => format!("{}'s results", self.call(call)),
            Cause::Submission => format!("{submit}'s submissions"),
            Cause::Receipt => format!("{submit}'s receipts"),
            Cause::ModelRequest => "model requests".to_owned(),
            Cause::ModelResponse => "model responses, which carry only what it sent".to_owned(),
            Cause::Outcome => "the outcome".to_owned(),
        }
    }
}

/// The items separated by commas, or `nothing`.
fn list(items: impl IntoIterator<Item = String>) -> String {
    or_nothing(&items.into_iter().collect::<Vec<_>>(), ", ")
}

/// The items joined by `separator`, or `nothing`.
fn or_nothing(items: &[String], separator: &str) -> String {
    if items.is_empty() {
        "nothing".to_owned()
    } else {
        items.join(separator)
    }
}

/// A section: a blank line, its heading, then its lines indented, or `empty` in their place.
fn section(heading: &str, empty: &str, lines: Vec<String>) -> Vec<String> {
    let lines = if lines.is_empty() {
        vec![empty.to_owned()]
    } else {
        lines
    };
    let indented = lines.into_iter().map(|line| format!("  {line}"));
    [String::new(), heading.to_owned()]
        .into_iter()
        .chain(indented)
        .collect()
}

fn rules(names: &Names<'_>, violations: &[Violation]) -> Vec<String> {
    let mut assets = Vec::new();
    let mut paths = Vec::new();
    for violation in violations {
        match violation {
            Violation::FinalModeAsset { role, asset } => {
                let kind = match role {
                    RoleId::Agent => "an agent",
                    RoleId::Judge(_) => "a train-mode judge",
                };
                assets.push(format!(
                    "{} declares the final-mode asset {asset} but is {kind}: move the asset to \
                     a final-mode judge",
                    names.role(*role)
                ));
            }
            Violation::FinalModeImage { role, holder } => {
                let kind = match role {
                    RoleId::Agent => "an agent",
                    RoleId::Judge(_) => "a train-mode judge",
                };
                assets.push(format!(
                    "{} runs the image of {}, which declares final-mode assets, but is {kind}: \
                     give it an image without them",
                    names.role(*role),
                    names.role(*holder),
                ));
            }
            Violation::FinalModePath { from, through, to } => {
                let path = iter::once(from).chain(through).chain(iter::once(to));
                let path: Vec<String> = path.map(|&node| names.node(node)).collect();
                paths.push(format!(
                    "final-mode {} reaches untrusted {} by {}: cut this path",
                    names.principal(from.principal),
                    names.principal(to.principal),
                    path.join(" -> "),
                ));
            }
        }
    }
    let rules = [
        (
            "final-mode assets only on final-mode judges, declared or in their images",
            assets,
        ),
        (
            "no information path from a final-mode judge to an untrusted principal live in the \
             same or a later phase",
            paths,
        ),
    ];
    let lines = rules.into_iter().flat_map(|(rule, broken)| {
        let verdict = if broken.is_empty() { "holds" } else { "broken" };
        let broken = broken.into_iter().map(|line| format!("  {line}"));
        iter::once(format!("{verdict}: {rule}")).chain(broken)
    });
    section("Rules", "", lines.collect())
}

fn assumptions(names: &Names<'_>, assumptions: &[Assumption]) -> Vec<String> {
    let authors = [
        (Author::Workflow, names.workflow().name().as_str()),
        (Author::Pumpkin, "Pumpkin"),
    ];
    let mut lines = Vec::new();
    for (author, author_name) in authors {
        let claims = assumptions
            .iter()
            .filter(|assumption| assumption.author() == author)
            .map(|assumption| assumption_line(names, assumption));
        let heading = format!("Assumed by {author_name}");
        lines.extend(section(&heading, "nothing", claims.collect()));
    }
    lines
}

fn assumption_line(names: &Names<'_>, assumption: &Assumption) -> String {
    match assumption {
        Assumption::Trusted(judge) => {
            let judge = names.role(RoleId::Judge(*judge));
            format!("{judge} is trusted: its own code acts as its author intends")
        }
        Assumption::DataSlot { judge, slot } => {
            let judge = names.role(RoleId::Judge(*judge));
            format!("{judge}.{slot} is a data slot: {judge} parses it without running it")
        }
        Assumption::Assets(role) => assets_line(names, *role),
        Assumption::ClosedRoute { role, models } => format!(
            "{}'s model route to {} is closed: one pinned upstream, no onward fetching, and no \
             state shared across requests",
            names.role(*role),
            list(models.as_slice().iter().map(ToString::to_string)),
        ),
        Assumption::TaskIsolation {
            call,
            judge,
            sources,
        } => format!(
            "{}'s results from {} rest on the task's own isolation inside {}'s sandbox, which \
             runs code from {}: that code cannot forge them",
            names.call(*call),
            names.node(*judge),
            names.principal(judge.principal),
            names.nodes(sources),
        ),
        Assumption::UntrustedResults { call, judge } => format!(
            "{}'s results from {} come from untrusted {}'s own code: the workflow takes them as \
             they are",
            names.call(*call),
            names.node(*judge),
            names.principal(judge.principal),
        ),
    }
}

fn assets_line(names: &Names<'_>, role: RoleId) -> String {
    let assets = &names.workflow().sandbox(role).assets;
    let role = names.role(role);
    if assets.is_empty() {
        return format!("{role} declares no assets: its image holds no final-mode data");
    }
    let assets = assets.iter().map(|asset| {
        let mode = match asset.mode {
            AssetMode::Any => "any",
            AssetMode::Final => "final",
        };
        format!("{} ({mode}-mode, at {})", asset.name, asset.path)
    });
    format!(
        "{role} declares {}: its image holds no other final-mode data",
        list(assets)
    )
}

fn principals(names: &Names<'_>) -> Vec<String> {
    let workflow = names.workflow();
    let live = graph::live(workflow);
    let live_in = |role: RoleId| {
        let phases = live.iter().filter(|(_, roles)| roles.contains(&role));
        let phases: Vec<&str> = phases.map(|(phase, _)| phase.name()).collect();
        if phases.is_empty() {
            "live in no phase".to_owned()
        } else {
            format!("live in {}", phases.join(", "))
        }
    };
    let agent = format!(
        "{}: an untrusted agent, {}",
        names.role(RoleId::Agent),
        live_in(RoleId::Agent)
    );
    let judges = workflow.judges().map(|(judge_id, judge)| {
        let role = RoleId::Judge(judge_id);
        let trust = match judge.trust {
            Trust::Trusted => "a trusted",
            Trust::Untrusted => "an untrusted",
        };
        let mode = match judge.mode {
            Mode::Train => "train",
            Mode::Final => "final",
        };
        let name = names.role(role);
        format!("{name}: {trust} {mode}-mode judge, {}", live_in(role))
    });
    let pattern = match workflow.main() {
        Main::SoloThenJudge(_) => "solo_then_judge",
    };
    let others = [
        format!("root: the pattern, {pattern}, trusted and live in every phase"),
        "operator: trusted, receives the outcome".to_owned(),
        "model provider: serves model routes".to_owned(),
    ];
    section(
        "Principals",
        "",
        iter::once(agent).chain(judges).chain(others).collect(),
    )
}

fn flows(names: &Names<'_>, report: &Report<'_>) -> Vec<String> {
    let mut lines = Vec::new();
    for phase in Phase::ALL {
        lines.push(format!("in {}:", phase.name()));
        let flows = report.flows.iter().filter(|flow| flow.from.phase == phase);
        let flows: Vec<String> = flows
            .map(|flow| {
                format!(
                    "  {} -> {} ({}): {}",
                    names.node(flow.from),
                    names.target(flow.to),
                    match flow.kind {
                        SlotKind::Code => "code",
                        SlotKind::Data => "data",
                    },
                    names.cause(flow.cause),
                )
            })
            .collect();
        lines.extend(flows);
        let transitions = report.transitions.iter().filter(|t| t.from == phase);
        lines.extend(transitions.map(|transition| {
            let survivors = transition.survivors.iter();
            let survivors = survivors.map(|&principal| names.principal(principal).to_owned());
            format!(
                "  then {}, survived by {}",
                transition.to.name(),
                list(survivors)
            )
        }));
    }
    section(
        "Flows, where r@p is r's instances in phase p, and a code flow gives control of its \
         receiver to its sender and to whatever reaches the sender",
        "",
        lines,
    )
}

fn authority(names: &Names<'_>, authority: &[Authority]) -> Vec<String> {
    let mut lines = Vec::new();
    for Authority {
        node,
        reaches,
        controls,
        uses,
    } in authority
    {
        let reaches = list(reaches.iter().map(|&target| names.target(target)));
        // Each chunk is a non-empty run of one node's abilities, the node's own first.
        let uses = uses.chunk_by(|a, b| a.by == b.by).map(|chunk| {
            let by = chunk[0].by;
            let abilities = list(chunk.iter().map(|u| names.ability(u.ability).to_owned()));
            if by == *node {
                abilities
            } else {
                format!("{}'s {abilities}", names.node(by))
            }
        });
        lines.extend([
            format!("{}:", names.node(*node)),
            format!("  reaches {reaches}"),
            format!("  controls {}", names.nodes(controls)),
            format!("  can use {}", or_nothing(&uses.collect::<Vec<_>>(), "; ")),
        ]);
    }
    section(
        "Authority, Auth(R): what each node reaches, controls, and can use itself or through \
         the nodes it controls",
        "",
        lines,
    )
}

fn untrusted_code(names: &Names<'_>, untrusted_code: &[UntrustedCode]) -> Vec<String> {
    let lines = untrusted_code
        .iter()
        .map(|UntrustedCode { node, sources }| {
            format!(
                "{} runs code from {}",
                names.node(*node),
                names.nodes(sources)
            )
        });
    section(
        "Untrusted code: the nodes that run an untrusted principal's code, and whose",
        "none",
        lines.collect(),
    )
}

fn trust_domains(names: &Names<'_>, domains: &[BTreeSet<Node>]) -> Vec<String> {
    let lines = domains.iter().map(|domain| names.nodes(domain));
    section(
        "Trust domains: the nodes that control each other",
        "none: no two nodes control each other",
        lines.collect(),
    )
}
