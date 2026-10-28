use std::collections::HashMap;

use super::{
    Agent, Judge, JudgeGrant, JudgeId, Main, RoleId, Sandbox, Score, SlotId, SoloThenJudge,
    SubmitGrant, Workflow,
};
use crate::ir::{self, Mode, Name};
use crate::sandbox::MAX_OP_PAYLOAD_BYTES;

mod errors;

use errors::{Errors, Reported};
pub use errors::{ParseError, ParseErrors};

impl Workflow {
    /// Parses a deserialized IR: resolves its names to ids and checks the rules that span its
    /// parts.
    ///
    /// # Errors
    ///
    /// Returns the rules the IR breaks, each with its path in the IR, such as a name that
    /// resolves to nothing or a second agent. A rule about parts that fail to resolve is not
    /// checked, so fixing those errors may reveal others.
    pub fn parse(ir: &ir::Workflow) -> Result<Self, ParseErrors> {
        Parser::default().workflow(ir)
    }
}

/// Role names that would read as other principals.
const RESERVED_ROLE_NAMES: [&str; 2] = ["root", "operator"];

/// Where a `solo_then_judge` pattern's errors are reported.
const PATTERN: &str = "main.solo_then_judge";

#[derive(Default)]
struct Parser<'ir> {
    errors: Errors,
    role_ids: HashMap<&'ir str, RoleId>,
    /// Every role with an agent runtime, each with its path in the IR.
    agents: Vec<(String, Agent)>,
    judges: Vec<Judge>,
}

/// A parsed `solo_then_judge` pattern, and the grants it gives the agent.
struct Pattern {
    judge_grants: Vec<JudgeGrant>,
    submit_grant: SubmitGrant,
    main: SoloThenJudge,
}

impl<'ir> Parser<'ir> {
    fn workflow(mut self, ir: &'ir ir::Workflow) -> Result<Workflow, ParseErrors> {
        let ir::Workflow {
            ir_version: ir::IrVersion::V0,
            name,
            roles,
            main,
        } = ir;
        self.roles(roles);
        let agent = self.the_agent();
        let pattern = match main {
            ir::Main::SoloThenJudge(pattern) => self.solo_then_judge(pattern),
        };
        let Self { errors, judges, .. } = self;
        let (agent, pattern) = errors.finish(agent.and_then(|agent| Ok((agent, pattern?))))?;
        let Pattern {
            judge_grants,
            submit_grant,
            main,
        } = pattern;
        Ok(Workflow {
            name: name.clone(),
            agent,
            judges,
            judge_grants,
            submit_grant,
            main: Main::SoloThenJudge(main),
        })
    }

    fn roles(&mut self, roles: &'ir [ir::Role]) {
        let labels = self
            .errors
            .unique("roles", roles.iter().map(|role| &role.name));
        for (role, path) in roles.iter().zip(labels) {
            let ir::Role {
                name,
                image,
                user,
                resources,
                runtime,
                egress: ir::Egress { model },
                assets,
            } = role;
            if RESERVED_ROLE_NAMES.contains(&name.as_str()) {
                self.errors.report(
                    format!("{path}.name"),
                    format!("`{name}` is reserved: use another name"),
                );
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
                ir::Runtime::Agent(agent) => self.agent(path, sandbox, agent),
                ir::Runtime::Judge(judge) => {
                    if model.is_some() {
                        self.errors.report(
                            format!("{path}.egress.model"),
                            format!(
                                "`{name}` is a judge, and only the agent has a model route: \
                                 remove it"
                            ),
                        );
                    }
                    self.judge(&path, sandbox, judge)
                }
            };
            self.role_ids.entry(name.as_str()).or_insert(id);
        }
    }

    fn agent(&mut self, path: String, sandbox: Sandbox, agent: &ir::AgentRuntime) -> RoleId {
        let ir::AgentRuntime { command } = agent;
        let agent = Agent {
            sandbox,
            command: command.clone(),
        };
        self.agents.push((path, agent));
        RoleId::Agent
    }

    /// The workflow's one agent. Reports a workflow with none, and each agent after the first.
    fn the_agent(&mut self) -> Result<Agent, Reported> {
        let mut agents = std::mem::take(&mut self.agents).into_iter();
        let Some((_, agent)) = agents.next() else {
            return Err(self
                .errors
                .report("roles", "no role is an agent: a workflow has exactly one"));
        };
        let first = agent.sandbox.name.clone();
        let mut parsed = Ok(agent);
        for (path, other) in agents {
            let message = format!(
                "`{}` is a second agent, after `{first}`: a workflow has exactly one",
                other.sandbox.name
            );
            parsed = Err(self.errors.report(format!("{path}.runtime"), message));
        }
        parsed
    }

    fn judge(&mut self, path: &str, sandbox: Sandbox, judge: &ir::JudgeRuntime) -> RoleId {
        let ir::JudgeRuntime {
            command,
            mode,
            trust,
            slots,
        } = judge;
        let labels = self.errors.unique(
            &format!("{path}.runtime.judge.slots"),
            slots.iter().map(|slot| &slot.name),
        );
        // Every payload reaches its judge through an op, which carries no more.
        for (slot, label) in slots.iter().zip(labels) {
            let max_bytes = slot.max_bytes.get();
            if max_bytes > MAX_OP_PAYLOAD_BYTES {
                self.errors.report(
                    format!("{label}.max_bytes"),
                    format!(
                        "slot `{}`'s `max_bytes` is {max_bytes}, over the op payload limit of \
                         {MAX_OP_PAYLOAD_BYTES}: give at most that",
                        slot.name
                    ),
                );
            }
        }
        self.judges.push(Judge {
            sandbox,
            command: command.clone(),
            mode: *mode,
            trust: *trust,
            slots: slots.clone(),
        });
        RoleId::Judge(JudgeId(self.judges.len() - 1))
    }

    fn solo_then_judge(&mut self, pattern: &'ir ir::SoloThenJudge) -> Result<Pattern, Reported> {
        let ir::SoloThenJudge {
            grants,
            score,
            deadline_seconds,
        } = pattern;
        let (judge_grants, submit_grant) = self.grants(grants);
        let score = self.score(score);
        Ok(Pattern {
            judge_grants: judge_grants?,
            submit_grant: submit_grant?,
            main: SoloThenJudge {
                score: score?,
                deadline_seconds: *deadline_seconds,
            },
        })
    }

    /// The agent's judge grants, and its one submit grant.
    fn grants(
        &mut self,
        grants: &'ir [ir::Grant],
    ) -> (
        Result<Vec<JudgeGrant>, Reported>,
        Result<SubmitGrant, Reported>,
    ) {
        let collection = format!("{PATTERN}.grants");
        let labels = self
            .errors
            .unique(&collection, grants.iter().map(ir::Grant::name));
        let mut judge_grants = Vec::new();
        let mut submit_grants = Vec::new();
        for (grant, path) in grants.iter().zip(labels) {
            match grant {
                ir::Grant::Judge(grant) => judge_grants.push(self.judge_grant(&path, grant)),
                ir::Grant::Submit(ir::SubmitGrant { name }) => submit_grants.push((path, name)),
            }
        }
        let judge_grants = judge_grants.into_iter().collect();
        (judge_grants, self.submit_grant(&collection, &submit_grants))
    }

    fn judge_grant(&mut self, path: &str, grant: &ir::JudgeGrant) -> Result<JudgeGrant, Reported> {
        let ir::JudgeGrant {
            name,
            role,
            slot,
            calls,
            cumulative_seconds,
            concurrency,
            call_timeout_seconds,
        } = grant;
        let slot = self.judge_slot(&format!("{path}.judge"), role, slot)?;
        Ok(JudgeGrant {
            name: name.clone(),
            slot,
            calls: *calls,
            cumulative_seconds: *cumulative_seconds,
            concurrency: *concurrency,
            call_timeout_seconds: *call_timeout_seconds,
        })
    }

    /// The agent's one submit grant, of those named at their paths. Reports none, and each
    /// after the first.
    fn submit_grant(
        &mut self,
        collection: &str,
        grants: &[(String, &Name)],
    ) -> Result<SubmitGrant, Reported> {
        let Some(((_, first), others)) = grants.split_first() else {
            return Err(self
                .errors
                .report(collection, "no submit grant: give the agent exactly one"));
        };
        let mut parsed = Ok(SubmitGrant {
            name: (*first).clone(),
        });
        for (path, other) in others {
            let message = format!(
                "`{other}` is a second submit grant, after `{first}`: give the agent exactly one"
            );
            parsed = Err(self.errors.report(format!("{path}.submit"), message));
        }
        parsed
    }

    /// The score: a slot of a final-mode judge.
    fn score(&mut self, score: &ir::Score) -> Result<Score, Reported> {
        let ir::Score { role, slot } = score;
        let path = format!("{PATTERN}.score");
        let slot = self.judge_slot(&path, role, slot)?;
        match self.judges[slot.judge.0].mode {
            Mode::Final => Ok(Score { slot }),
            Mode::Train => Err(self.errors.report(
                format!("{path}.role"),
                format!("`{role}` is train-mode: name a final-mode judge"),
            )),
        }
    }

    /// Resolves a judge call's slot, `slot` of the judge `role`, with the call at `path`.
    fn judge_slot(&mut self, path: &str, role: &Name, slot: &Name) -> Result<SlotId, Reported> {
        let role_path = format!("{path}.role");
        let judge = match self
            .errors
            .resolve(&role_path, role, &self.role_ids, "role")?
        {
            RoleId::Judge(judge) => judge,
            RoleId::Agent => {
                return Err(self
                    .errors
                    .report(role_path, format!("`{role}` is an agent: name a judge")));
            }
        };
        let index = self.judges[judge.0]
            .slots
            .iter()
            .position(|judge_slot| judge_slot.name == *slot);
        match index {
            Some(index) => Ok(SlotId { judge, index }),
            None => Err(self.errors.report(
                format!("{path}.slot"),
                format!("`{role}` has no slot named `{slot}`: name one of its slots"),
            )),
        }
    }
}
