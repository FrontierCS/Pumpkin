use std::collections::BTreeMap;
use std::time::Duration;

use super::errors::{Errors, Reported};
use super::{
    Agent, Judge, JudgeGrant, JudgeId, Main, ParseErrors, RoleId, Sandbox, Score, SlotId,
    SoloThenJudge, SubmitGrant, Workflow,
};
use crate::ir::{self, JsonPointer, Name, RolePath};
use crate::sandbox::MAX_OP_PAYLOAD_BYTES;

impl Workflow {
    /// Parses a deserialized IR: resolves its names to ids and checks the rules that span its
    /// parts.
    ///
    /// # Errors
    ///
    /// Returns the rules the IR breaks, each [at](super::ParseError::at) its place in the IR,
    /// such as a name that resolves to nothing or a second agent. A rule about parts that fail
    /// to resolve is not checked, so fixing those errors may reveal others.
    pub fn parse(ir: &ir::Workflow) -> Result<Self, ParseErrors> {
        Parser::default().workflow(ir)
    }
}

/// Where a `solo_then_judge` pattern's errors are reported.
fn pattern() -> JsonPointer {
    JsonPointer::root().token("main").token("solo_then_judge")
}

#[derive(Default)]
struct Parser<'ir> {
    errors: Errors,
    role_ids: BTreeMap<&'ir str, RoleId>,
    /// Every role with an agent runtime, each at its place in the IR.
    agents: Vec<(JsonPointer, Agent)>,
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
            ir: ir.clone(),
            name: name.clone(),
            agent,
            judges,
            judge_grants,
            submit_grant,
            main: Main::SoloThenJudge(main),
        })
    }

    fn roles(&mut self, roles: &'ir [ir::Role]) {
        let list = JsonPointer::root().token("roles");
        let names = roles.iter().map(|role| &role.name);
        self.errors.unique(names, |index| list.token(index));
        for (index, role) in roles.iter().enumerate() {
            let at = list.token(index);
            let ir::Role {
                name,
                image,
                user,
                needs,
                runtime,
                egress: ir::Egress { model },
            } = role;
            let sandbox = Sandbox {
                name: name.clone(),
                image: image.clone(),
                user: *user,
                needs: *needs,
                at: at.clone(),
                model_route: model.clone(),
            };
            let id = match runtime {
                ir::Runtime::Agent(agent) => self.agent(at, sandbox, agent),
                ir::Runtime::Judge(judge) => {
                    if model.is_some() {
                        self.errors.report(
                            at.token("egress").token("model"),
                            format!(
                                "`{name}` is a judge, and only the agent has a model route: \
                                 remove it"
                            ),
                        );
                    }
                    self.judge(&at, sandbox, judge)
                }
            };
            self.role_ids.entry(name.as_str()).or_insert(id);
        }
    }

    fn agent(&mut self, at: JsonPointer, sandbox: Sandbox, agent: &ir::AgentRuntime) -> RoleId {
        let ir::AgentRuntime {
            command,
            dirs,
            files,
            stdin,
        } = agent;
        self.agent_paths(&at.token("runtime").token("agent"), dirs, files);
        let mut dirs = dirs.clone();
        dirs.sort_by(|a, b| a.path.cmp(&b.path));
        let agent = Agent {
            sandbox,
            command: command.clone(),
            dirs,
            files: files.clone(),
            stdin: stdin.clone(),
        };
        self.agents.push((at, agent));
        RoleId::Agent
    }

    /// Reports each path of the agent's `dirs` and `files`, in the runtime at `runtime`, that
    /// comes again, or is below a file.
    fn agent_paths(&mut self, runtime: &JsonPointer, dirs: &[ir::Dir], files: &[ir::File]) {
        let file_paths = files.iter().map(|file| &file.path);
        let all: Vec<&RolePath> = dirs.iter().map(|dir| &dir.path).chain(file_paths).collect();
        // The place of the entry at `index` in `all`: a dir, or past them, a file.
        let at = |index: usize| match index.checked_sub(dirs.len()) {
            None => runtime.token("dirs").token(index),
            Some(file) => runtime.token("files").token(file),
        };
        for (index, path) in all.iter().enumerate() {
            let earlier = all[..index].iter().position(|earlier| earlier == path);
            let file = (dirs.len()..all.len()).find(|&file| path.is_below(all[file]));
            let message = match (earlier, file) {
                (Some(earlier), _) => {
                    let earlier = at(earlier);
                    format!("`{path}` is also the path of {earlier}: put each path once")
                }
                (None, Some(file)) => format!(
                    "`{path}` is below `{}`, the file at {}: put it in a directory",
                    all[file],
                    at(file)
                ),
                (None, None) => continue,
            };
            self.errors.report(at(index).token("path"), message);
        }
    }

    /// The workflow's one agent. Reports a workflow with none, and each agent after the first.
    fn the_agent(&mut self) -> Result<Agent, Reported> {
        let mut agents = std::mem::take(&mut self.agents).into_iter();
        let Some((_, agent)) = agents.next() else {
            return Err(self.errors.report(
                JsonPointer::root().token("roles"),
                "no role is an agent: a workflow has exactly one",
            ));
        };
        let first = agent.sandbox.name.clone();
        let mut parsed = Ok(agent);
        for (at, other) in agents {
            let message = format!(
                "`{}` is a second agent, after `{first}`: a workflow has exactly one",
                other.sandbox.name
            );
            parsed = Err(self.errors.report(at.token("runtime"), message));
        }
        parsed
    }

    fn judge(&mut self, at: &JsonPointer, sandbox: Sandbox, judge: &ir::JudgeRuntime) -> RoleId {
        let ir::JudgeRuntime {
            command,
            slots,
            result,
        } = judge;
        let list = at.token("runtime").token("judge").token("slots");
        let names = slots.iter().map(|slot| &slot.name);
        self.errors.unique(names, |index| list.token(index));
        // Every payload reaches its judge through an op, which carries no more.
        for (index, slot) in slots.iter().enumerate() {
            let max_bytes = slot.max_bytes.get();
            if max_bytes > MAX_OP_PAYLOAD_BYTES {
                self.errors.report(
                    list.token(index).token("max_bytes"),
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
            slots: slots.clone(),
            result: result.clone(),
        });
        RoleId::Judge(JudgeId(self.judges.len() - 1))
    }

    fn solo_then_judge(&mut self, pattern: &'ir ir::SoloThenJudge) -> Result<Pattern, Reported> {
        let ir::SoloThenJudge {
            grants,
            score,
            deadline_seconds,
            finalize_grace_seconds,
        } = pattern;
        let (judge_grants, submit_grant) = self.grants(grants);
        let score = self.score(score, grants);
        Ok(Pattern {
            judge_grants: judge_grants?,
            submit_grant: submit_grant?,
            main: SoloThenJudge {
                score: score?,
                deadline: Duration::from_secs(deadline_seconds.get()),
                finalize_grace: Duration::from_secs(*finalize_grace_seconds),
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
        let list = pattern().token("grants");
        // A grant's fields are below its kind, as in `{"judge": {…}}`.
        let place = |index: usize| {
            let kind = match grants[index] {
                ir::Grant::Judge(_) => "judge",
                ir::Grant::Submit(_) => "submit",
            };
            list.token(index).token(kind)
        };
        self.errors
            .unique(grants.iter().map(ir::Grant::name), place);
        let mut judge_grants = Vec::new();
        let mut submit_grants = Vec::new();
        for (index, grant) in grants.iter().enumerate() {
            let at = place(index);
            match grant {
                ir::Grant::Judge(grant) => {
                    judge_grants.push(self.judge_grant(&at, grant, &grants[..index]));
                }
                ir::Grant::Submit(ir::SubmitGrant { name }) => submit_grants.push((at, name)),
            }
        }
        let judge_grants = judge_grants.into_iter().collect();
        (judge_grants, self.submit_grant(&list, &submit_grants))
    }

    /// A judge grant, at `at`, whose judge none of the `earlier` grants names.
    fn judge_grant(
        &mut self,
        at: &JsonPointer,
        grant: &ir::JudgeGrant,
        earlier: &[ir::Grant],
    ) -> Result<JudgeGrant, Reported> {
        let ir::JudgeGrant {
            name,
            role,
            slot,
            calls,
            cumulative_seconds,
            call_timeout_seconds,
        } = grant;
        let shared = match self.judge_grant_naming(role, earlier) {
            Some(first) => Err(self.errors.report(
                at.token("role"),
                format!(
                    "`{role}` is named by judge grants `{}` and `{name}`: name each judge from \
                     one grant",
                    first.name
                ),
            )),
            None => Ok(()),
        };
        let slot = self.judge_slot(at, role, slot)?;
        shared.map(|()| JudgeGrant {
            name: name.clone(),
            slot,
            calls: *calls,
            cumulative_seconds: *cumulative_seconds,
            call_timeout_seconds: *call_timeout_seconds,
        })
    }

    /// The agent's one submit grant, of those named at their places in the list at `list`.
    /// Reports none, and each after the first.
    fn submit_grant(
        &mut self,
        list: &JsonPointer,
        grants: &[(JsonPointer, &Name)],
    ) -> Result<SubmitGrant, Reported> {
        let Some(((_, first), others)) = grants.split_first() else {
            return Err(self
                .errors
                .report(list.clone(), "no submit grant: give the agent exactly one"));
        };
        let mut parsed = Ok(SubmitGrant {
            name: (*first).clone(),
        });
        for (at, other) in others {
            let message = format!(
                "`{other}` is a second submit grant, after `{first}`: give the agent exactly one"
            );
            parsed = Err(self.errors.report(at.clone(), message));
        }
        parsed
    }

    /// The score: a slot of a judge that none of the agent's `grants` names, so that the judge
    /// that scores the selection returns nothing to the agent.
    fn score(&mut self, score: &ir::Score, grants: &[ir::Grant]) -> Result<Score, Reported> {
        let ir::Score {
            role,
            slot,
            timeout_seconds,
        } = score;
        let at = pattern().token("score");
        let shared = match self.judge_grant_naming(role, grants) {
            Some(grant) => Err(self.errors.report(
                at.token("role"),
                format!(
                    "`{role}` is also the role of judge grant `{}`: name a judge no grant names",
                    grant.name
                ),
            )),
            None => Ok(()),
        };
        let slot = self.judge_slot(&at, role, slot)?;
        shared.map(|()| Score {
            slot,
            timeout: Duration::from_secs(timeout_seconds.get()),
        })
    }

    /// The first of `grants` that is a judge grant naming `role`, if `role` is a judge.
    fn judge_grant_naming<'g>(
        &self,
        role: &Name,
        grants: &'g [ir::Grant],
    ) -> Option<&'g ir::JudgeGrant> {
        // The grants are compared by the role they name in the IR, so that a rule about the
        // judges they name holds even where a grant's slot or the score's fails to resolve.
        if !matches!(self.role_ids.get(role.as_str()), Some(RoleId::Judge(_))) {
            return None;
        }
        grants.iter().find_map(|grant| match grant {
            ir::Grant::Judge(grant) => (grant.role == *role).then_some(grant),
            ir::Grant::Submit(_) => None,
        })
    }

    /// Resolves a judge call's slot, `slot` of the judge `role`, with the call at `at`.
    fn judge_slot(
        &mut self,
        at: &JsonPointer,
        role: &Name,
        slot: &Name,
    ) -> Result<SlotId, Reported> {
        let role_at = at.token("role");
        let judge = match self
            .errors
            .resolve(&role_at, role, &self.role_ids, "role")?
        {
            RoleId::Judge(judge) => judge,
            RoleId::Agent => {
                return Err(self
                    .errors
                    .report(role_at, format!("`{role}` is an agent: name a judge")));
            }
        };
        let index = self.judges[judge.0]
            .slots
            .iter()
            .position(|judge_slot| judge_slot.name == *slot);
        match index {
            Some(index) => Ok(SlotId { judge, index }),
            None => Err(self.errors.report(
                at.token("slot"),
                format!("`{role}` has no slot named `{slot}`: name one of its slots"),
            )),
        }
    }
}
