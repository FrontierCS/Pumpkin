use std::collections::{BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::fmt;

use super::{
    Agent, AgentId, Holder, Judge, JudgeGrant, JudgeGrantId, JudgeId, Main, Phase, PhaseId, RoleId,
    Sandbox, SlotId, SoloThenJudge, SubmitGrant, SubmitGrantId, Workflow,
};
use crate::ir::{self, Mode, Name};

/// A rule an IR breaks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// Where in the IR, such as `grants[train].holder`.
    pub path: String,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

impl Error for ParseError {}

/// Every rule an IR breaks, in the order of the parts that break them. Never empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseErrors(Vec<ParseError>);

impl ParseErrors {
    pub fn as_slice(&self) -> &[ParseError] {
        &self.0
    }
}

impl fmt::Display for ParseErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut errors = self.0.iter();
        if let Some(first) = errors.next() {
            write!(f, "{first}")?;
        }
        for error in errors {
            write!(f, "\n{error}")?;
        }
        Ok(())
    }
}

impl Error for ParseErrors {}

impl Workflow {
    /// Parses a deserialized IR: resolves its names to ids, applies its defaults, and checks
    /// the rules that span its parts.
    ///
    /// # Errors
    ///
    /// Returns every rule the IR breaks, each with its path in the IR, such as a name that
    /// resolves to nothing or a grant whose holder is not live where it applies.
    pub fn parse(ir: &ir::Workflow) -> Result<Self, ParseErrors> {
        Parser::default().workflow(ir)
    }
}

/// Role names that would read as other principals.
const RESERVED_ROLE_NAMES: [&str; 2] = ["root", "operator"];

/// A grant of either op.
#[derive(Clone, Copy)]
enum GrantId {
    Judge(JudgeGrantId),
    Submit(SubmitGrantId),
}

#[derive(Default)]
struct Errors(Vec<ParseError>);

impl Errors {
    fn report(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.0.push(ParseError {
            path: path.into(),
            message: message.into(),
        });
    }

    /// Reports each name that repeats an earlier one in a collection.
    fn unique<'n>(&mut self, collection: &str, names: impl IntoIterator<Item = &'n Name>) {
        let mut seen = HashSet::new();
        for name in names {
            if !seen.insert(name) {
                self.report(format!("{collection}[{name}]"), "the name is already used");
            }
        }
    }

    /// Resolves a list of references to a set, reporting repeated and unknown names.
    fn resolve_all<Id: Copy + Ord>(
        &mut self,
        path: &str,
        names: &[Name],
        ids: &HashMap<&str, Id>,
        kind: &str,
    ) -> BTreeSet<Id> {
        let mut seen = HashSet::new();
        let mut resolved = BTreeSet::new();
        for name in names {
            if !seen.insert(name) {
                self.report(path, format!("`{name}` is already listed"));
            } else if let Some(&id) = ids.get(name.as_str()) {
                resolved.insert(id);
            } else {
                self.report(path, format!("no {kind} is named `{name}`"));
            }
        }
        resolved
    }
}

#[derive(Default)]
struct Parser<'ir> {
    errors: Errors,
    role_ids: HashMap<&'ir str, RoleId>,
    phase_ids: HashMap<&'ir str, PhaseId>,
    /// `None` for a grant that failed to parse, which has reported why.
    grant_ids: HashMap<&'ir str, Option<GrantId>>,
    agents: Vec<Agent>,
    judges: Vec<Judge>,
    phases: Vec<Phase>,
    judge_grants: Vec<JudgeGrant>,
    submit_grants: Vec<SubmitGrant>,
}

impl<'ir> Parser<'ir> {
    fn workflow(mut self, ir: &'ir ir::Workflow) -> Result<Workflow, ParseErrors> {
        let ir::Workflow {
            ir_version: ir::IrVersion::V0,
            name,
            roles,
            grants,
            phases,
            main,
        } = ir;
        self.roles(roles);
        self.phases(phases);
        self.grants(grants);
        let main = match main {
            ir::Main::SoloThenJudge(pattern) => {
                self.solo_then_judge(pattern).map(Main::SoloThenJudge)
            }
        };
        match main {
            Some(main) if self.errors.0.is_empty() => Ok(Workflow {
                name: name.clone(),
                agents: self.agents,
                judges: self.judges,
                judge_grants: self.judge_grants,
                submit_grants: self.submit_grants,
                phases: self.phases,
                main,
            }),
            // Whatever left `main` unparsed has reported why.
            Some(_) | None => Err(ParseErrors(self.errors.0)),
        }
    }

    fn roles(&mut self, roles: &'ir [ir::Role]) {
        self.errors
            .unique("roles", roles.iter().map(|role| &role.name));
        for role in roles {
            let ir::Role {
                name,
                image,
                user,
                resources,
                runtime,
                egress: ir::Egress { model },
                assets,
            } = role;
            let path = format!("roles[{name}]");
            if RESERVED_ROLE_NAMES.contains(&name.as_str()) {
                self.errors
                    .report(format!("{path}.name"), format!("`{name}` is reserved"));
            }
            self.errors.unique(
                &format!("{path}.assets"),
                assets.iter().map(|asset| &asset.name),
            );
            let sandbox = Sandbox {
                name: name.clone(),
                image: image.clone(),
                user: *user,
                resources: *resources,
                model_route: model.clone(),
                assets: assets.clone(),
            };
            let id = match runtime {
                ir::Runtime::Agent(agent) => self.agent(sandbox, agent),
                ir::Runtime::Judge(judge) => self.judge(&path, sandbox, judge),
            };
            self.role_ids.entry(name.as_str()).or_insert(id);
        }
    }

    fn agent(&mut self, sandbox: Sandbox, agent: &ir::AgentRuntime) -> RoleId {
        let ir::AgentRuntime { command } = agent;
        self.agents.push(Agent {
            sandbox,
            command: command.clone(),
        });
        RoleId::Agent(AgentId(self.agents.len() - 1))
    }

    fn judge(&mut self, path: &str, sandbox: Sandbox, judge: &ir::JudgeRuntime) -> RoleId {
        let ir::JudgeRuntime {
            command,
            mode,
            trust,
            slots,
        } = judge;
        self.errors.unique(
            &format!("{path}.runtime.judge.slots"),
            slots.iter().map(|slot| &slot.name),
        );
        self.judges.push(Judge {
            sandbox,
            command: command.clone(),
            mode: *mode,
            trust: *trust,
            slots: slots.clone(),
        });
        RoleId::Judge(JudgeId(self.judges.len() - 1))
    }

    fn phases(&mut self, phases: &'ir [ir::Phase]) {
        if phases.is_empty() {
            self.errors
                .report("phases", "a workflow has at least one phase");
        }
        self.errors
            .unique("phases", phases.iter().map(|phase| &phase.name));
        for (index, phase) in phases.iter().enumerate() {
            self.phase_ids
                .entry(phase.name.as_str())
                .or_insert(PhaseId(index));
        }
        for (index, phase) in phases.iter().enumerate() {
            let ir::Phase { name, live, next } = phase;
            let path = format!("phases[{name}]");
            let live =
                self.errors
                    .resolve_all(&format!("{path}.live"), live, &self.role_ids, "role");
            let next =
                self.errors
                    .resolve_all(&format!("{path}.next"), next, &self.phase_ids, "phase");
            if next.contains(&PhaseId(index)) {
                self.errors
                    .report(format!("{path}.next"), "a phase does not follow itself");
            }
            self.phases.push(Phase {
                name: name.clone(),
                live,
                next,
            });
        }
    }

    fn grants(&mut self, grants: &'ir [ir::Grant]) {
        self.errors
            .unique("grants", grants.iter().map(|grant| &grant.name));
        for grant in grants {
            let ir::Grant {
                name,
                holder,
                phases,
                op,
            } = grant;
            let path = format!("grants[{name}]");
            if phases.is_empty() {
                self.errors.report(
                    format!("{path}.phases"),
                    "a grant applies in at least one phase",
                );
            }
            let phase_ids = self.errors.resolve_all(
                &format!("{path}.phases"),
                phases,
                &self.phase_ids,
                "phase",
            );
            let holder = match holder {
                ir::Principal::Root => Some(Holder::Root),
                ir::Principal::Role(role) => {
                    let holder_path = format!("{path}.holder");
                    let id = self.role(&holder_path, role);
                    if let Some(id) = id {
                        self.require_live(&holder_path, id, role, &phase_ids);
                    }
                    id.map(Holder::Role)
                }
            };
            let id = match op {
                ir::Op::Judge(op) => self
                    .judge_grant(&path, name, holder, phase_ids, op)
                    .map(GrantId::Judge),
                ir::Op::Submit(op) => self
                    .submit_grant(&path, name, holder, phase_ids, *op)
                    .map(GrantId::Submit),
            };
            self.grant_ids.entry(name.as_str()).or_insert(id);
        }
    }

    fn judge_grant(
        &mut self,
        path: &str,
        name: &Name,
        holder: Option<Holder>,
        phases: BTreeSet<PhaseId>,
        op: &ir::JudgeOp,
    ) -> Option<JudgeGrantId> {
        let ir::JudgeOp {
            role,
            slot,
            calls,
            cumulative_seconds,
            concurrency,
            call_timeout_seconds,
        } = op;
        let role_path = format!("{path}.op.judge.role");
        let judge = match self.role(&role_path, role)? {
            RoleId::Judge(judge) => judge,
            RoleId::Agent(_) => {
                self.errors
                    .report(role_path, format!("`{role}` is an agent, not a judge"));
                return None;
            }
        };
        self.require_live(&role_path, RoleId::Judge(judge), role, &phases);
        let Some(index) = self.judges[judge.0]
            .slots
            .iter()
            .position(|judge_slot| judge_slot.name == *slot)
        else {
            self.errors.report(
                format!("{path}.op.judge.slot"),
                format!("`{role}` has no slot named `{slot}`"),
            );
            return None;
        };
        self.judge_grants.push(JudgeGrant {
            name: name.clone(),
            holder: holder?,
            phases,
            slot: SlotId { judge, index },
            calls: *calls,
            cumulative_seconds: *cumulative_seconds,
            concurrency: *concurrency,
            call_timeout_seconds: *call_timeout_seconds,
        });
        Some(JudgeGrantId(self.judge_grants.len() - 1))
    }

    fn submit_grant(
        &mut self,
        path: &str,
        name: &Name,
        holder: Option<Holder>,
        phases: BTreeSet<PhaseId>,
        op: ir::SubmitOp,
    ) -> Option<SubmitGrantId> {
        let ir::SubmitOp { max_bytes } = op;
        let holder = match holder? {
            Holder::Role(role) => role,
            Holder::Root => {
                self.errors
                    .report(format!("{path}.holder"), "only a role holds a submit grant");
                return None;
            }
        };
        self.submit_grants.push(SubmitGrant {
            name: name.clone(),
            holder,
            phases,
            max_bytes,
        });
        Some(SubmitGrantId(self.submit_grants.len() - 1))
    }

    /// Reports each of a grant's phases in which a role is not live.
    fn require_live(
        &mut self,
        path: &str,
        role: RoleId,
        role_name: &Name,
        phases: &BTreeSet<PhaseId>,
    ) {
        for &phase in phases {
            let phase = &self.phases[phase.0];
            if !phase.is_live(role) {
                self.errors.report(
                    path,
                    format!("`{role_name}` is not live in phase `{}`", phase.name),
                );
            }
        }
    }

    fn role(&mut self, path: &str, name: &Name) -> Option<RoleId> {
        let role = self.role_ids.get(name.as_str()).copied();
        if role.is_none() {
            self.errors
                .report(path, format!("no role is named `{name}`"));
        }
        role
    }

    fn phase(&mut self, path: &str, name: &Name) -> Option<PhaseId> {
        let phase = self.phase_ids.get(name.as_str()).copied();
        if phase.is_none() {
            self.errors
                .report(path, format!("no phase is named `{name}`"));
        }
        phase
    }

    /// Resolves a grant by name. `None` if the grant is unknown, or failed to parse.
    fn grant(&mut self, path: &str, name: &Name) -> Option<GrantId> {
        let Some(&id) = self.grant_ids.get(name.as_str()) else {
            self.errors
                .report(path, format!("no grant is named `{name}`"));
            return None;
        };
        id
    }

    fn solo_then_judge(&mut self, pattern: &ir::SoloThenJudge) -> Option<SoloThenJudge> {
        let path = "main.solo_then_judge";
        // Resolve every reference first, so that the rules below see only parsed parts.
        let agent = match self.role(&format!("{path}.agent"), &pattern.agent) {
            Some(RoleId::Agent(agent)) => Some(agent),
            Some(RoleId::Judge(_)) => {
                self.errors.report(
                    format!("{path}.agent"),
                    format!("`{}` is a judge, not an agent", pattern.agent),
                );
                None
            }
            None => None,
        };
        let submit = match self.grant(&format!("{path}.submit"), &pattern.submit) {
            Some(GrantId::Submit(submit)) => Some(submit),
            Some(GrantId::Judge(_)) => {
                self.errors.report(
                    format!("{path}.submit"),
                    format!("`{}` is not a submit grant", pattern.submit),
                );
                None
            }
            None => None,
        };
        let score = match self.grant(&format!("{path}.score"), &pattern.score) {
            Some(GrantId::Judge(score)) => Some(score),
            Some(GrantId::Submit(_)) => {
                self.errors.report(
                    format!("{path}.score"),
                    format!("`{}` is not a judge grant", pattern.score),
                );
                None
            }
            None => None,
        };
        let work_phase = self.phase(&format!("{path}.work_phase"), &pattern.work_phase);
        let final_phase = self.phase(&format!("{path}.final_phase"), &pattern.final_phase);
        let parsed = SoloThenJudge {
            agent: agent?,
            submit: submit?,
            score: score?,
            work_phase: work_phase?,
            final_phase: final_phase?,
            deadline_seconds: pattern.deadline_seconds,
        };
        let errors = self.errors.0.len();
        self.pattern_phases(path, pattern, &parsed);
        self.pattern_grants(path, pattern, &parsed);
        (self.errors.0.len() == errors).then_some(parsed)
    }

    /// The agent works in the initial phase, which is followed by the final phase, and is gone
    /// by the final phase.
    fn pattern_phases(&mut self, path: &str, pattern: &ir::SoloThenJudge, parsed: &SoloThenJudge) {
        let ir::SoloThenJudge {
            agent: agent_name,
            work_phase: work_name,
            final_phase: final_name,
            ..
        } = pattern;
        if parsed.work_phase == parsed.final_phase {
            self.errors.report(
                format!("{path}.final_phase"),
                "the final phase differs from the work phase",
            );
            return;
        }
        let agent = RoleId::Agent(parsed.agent);
        let work = &self.phases[parsed.work_phase.0];
        let final_ = &self.phases[parsed.final_phase.0];
        if parsed.work_phase != PhaseId(0) {
            self.errors.report(
                format!("{path}.work_phase"),
                format!("`{work_name}` is not the first phase, in which a run starts"),
            );
        }
        if !work.next.contains(&parsed.final_phase) {
            self.errors.report(
                format!("{path}.final_phase"),
                format!("the work phase `{work_name}` is not followed by `{final_name}`"),
            );
        }
        if !work.is_live(agent) {
            self.errors.report(
                format!("{path}.agent"),
                format!("`{agent_name}` is not live in the work phase `{work_name}`"),
            );
        }
        if final_.is_live(agent) {
            self.errors.report(
                format!("{path}.agent"),
                format!(
                    "`{agent_name}` is live in the final phase `{final_name}`, so it would \
                     survive into judging"
                ),
            );
        }
    }

    /// The agent submits in the work phase, and the root scores in the final phase with a
    /// final-mode judge whose slot fits any submission.
    fn pattern_grants(&mut self, path: &str, pattern: &ir::SoloThenJudge, parsed: &SoloThenJudge) {
        let ir::SoloThenJudge {
            agent: agent_name,
            submit: submit_name,
            score: score_name,
            work_phase: work_name,
            final_phase: final_name,
            ..
        } = pattern;
        let submit = &self.submit_grants[parsed.submit.0];
        if submit.holder != RoleId::Agent(parsed.agent) {
            self.errors.report(
                format!("{path}.submit"),
                format!("`{submit_name}` is not held by the agent `{agent_name}`"),
            );
        }
        if !submit.phases.contains(&parsed.work_phase) {
            self.errors.report(
                format!("{path}.submit"),
                format!("`{submit_name}` does not apply in the work phase `{work_name}`"),
            );
        }
        let score = &self.judge_grants[parsed.score.0];
        if score.holder != Holder::Root {
            self.errors.report(
                format!("{path}.score"),
                format!("`{score_name}` is not held by the root"),
            );
        }
        if !score.phases.contains(&parsed.final_phase) {
            self.errors.report(
                format!("{path}.score"),
                format!("`{score_name}` does not apply in the final phase `{final_name}`"),
            );
        }
        let judge = &self.judges[score.slot.judge.0];
        match judge.mode {
            Mode::Final => {}
            Mode::Train => self.errors.report(
                format!("{path}.score"),
                format!(
                    "`{score_name}` judges with `{}`, which is train-mode",
                    judge.sandbox.name
                ),
            ),
        }
        let slot = &judge.slots[score.slot.index];
        if submit.max_bytes > slot.max_bytes {
            self.errors.report(
                format!("{path}.submit"),
                format!(
                    "submissions of up to {} bytes do not fit the score slot's {} bytes",
                    submit.max_bytes, slot.max_bytes
                ),
            );
        }
    }
}
