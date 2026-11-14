use std::collections::BTreeMap;
use std::time::Duration;

use super::errors::{Errors, Reported};
use super::{
    Agent, Judge, JudgeGrant, JudgeId, ParseErrors, RoleId, Sandbox, Score, Slot, SlotId,
    SubmitGrant, Workflow,
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

#[derive(Default)]
struct Parser<'ir> {
    errors: Errors,
    role_ids: BTreeMap<&'ir str, RoleId>,
    /// Each role's sandbox, in the IR's order.
    roles: Vec<Sandbox>,
    /// Every role with an agent runtime, each at its place in the IR, with the grants it holds.
    agents: Vec<(JsonPointer, Agent, &'ir [ir::Grant])>,
    judges: BTreeMap<RoleId, Judge>,
}

/// The agent's grants, parsed: its judge grants and its one submit grant.
type Grants = (
    Result<Vec<JudgeGrant>, Reported>,
    Result<SubmitGrant, Reported>,
);

impl<'ir> Parser<'ir> {
    fn workflow(mut self, ir: &'ir ir::Workflow) -> Result<Workflow, ParseErrors> {
        let ir::Workflow {
            ir_version: ir::IrVersion::V0,
            name: _,
            roles,
            score,
        } = ir;
        self.roles(roles);
        // With no agent, it holds no grants, so the score is checked against none.
        let (agent, grants) = match self.first_agent() {
            Ok((at, agent, grants)) => (Ok((agent, self.grants(&at, grants))), grants),
            Err(reported) => (Err(reported), &[][..]),
        };
        let score = self.score(score, grants);
        let Self {
            errors,
            roles,
            judges,
            ..
        } = self;
        let parsed = agent.and_then(|(agent, (judge_grants, submit_grant))| {
            Ok((agent, judge_grants?, submit_grant?, score?))
        });
        let (agent, judge_grants, submit_grant, score) = errors.finish(parsed)?;
        Ok(Workflow {
            ir: ir.clone(),
            roles,
            agent,
            judges,
            judge_grants,
            submit_grant,
            score,
        })
    }

    fn roles(&mut self, roles: &'ir [ir::Role]) {
        let list = JsonPointer::root().token("roles");
        let names = roles.iter().map(|role| &role.name);
        self.errors.unique(names, |index| list.token(index));
        for (index, role) in roles.iter().enumerate() {
            let (at, id) = (list.token(index), RoleId(index));
            let ir::Role {
                name,
                image,
                user,
                needs,
                runtime,
                egress: ir::Egress { model },
                grants,
            } = role;
            self.roles.push(Sandbox {
                name: name.clone(),
                image: image.clone(),
                user: *user,
                needs: needs.clone(),
                at: at.clone(),
                model_route: model.clone(),
            });
            match runtime {
                ir::Runtime::Agent(agent) => self.agent(at, id, agent, grants),
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
                    if !grants.is_empty() {
                        self.errors.report(
                            at.token("grants"),
                            format!(
                                "`{name}` is a judge, and judges hold no grants yet: remove them"
                            ),
                        );
                    }
                    self.judge(&at, id, judge);
                }
            }
            self.role_ids.entry(name.as_str()).or_insert(id);
        }
    }

    fn agent(
        &mut self,
        at: JsonPointer,
        role: RoleId,
        agent: &ir::AgentRuntime,
        grants: &'ir [ir::Grant],
    ) {
        let ir::AgentRuntime {
            command,
            dirs,
            files,
            stdin,
            deadline_seconds,
            finalize_grace_seconds,
        } = agent;
        self.agent_paths(&at.token("runtime").token("agent"), dirs, files);
        let mut dirs = dirs.clone();
        dirs.sort_by(|a, b| a.path.cmp(&b.path));
        let agent = Agent {
            role,
            command: command.clone(),
            dirs,
            files: files.clone(),
            stdin: stdin.clone(),
            deadline: Duration::from_secs(deadline_seconds.get()),
            finalize_grace: Duration::from_secs(*finalize_grace_seconds),
        };
        self.agents.push((at, agent, grants));
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

    /// The workflow's first agent, at its place in the IR, with the grants it holds. Reports a
    /// workflow with none, and each agent after the first, so a workflow has exactly one.
    fn first_agent(&mut self) -> Result<(JsonPointer, Agent, &'ir [ir::Grant]), Reported> {
        let mut agents = std::mem::take(&mut self.agents).into_iter();
        let Some(first) = agents.next() else {
            return Err(self.errors.report(
                JsonPointer::root().token("roles"),
                "no role is an agent: a workflow has exactly one",
            ));
        };
        let name = |agent: &Agent| &self.roles[agent.role.0].name;
        for (at, other, _) in agents {
            let message = format!(
                "`{}` is a second agent, after `{}`: a workflow has exactly one",
                name(&other),
                name(&first.1)
            );
            self.errors.report(at.token("runtime"), message);
        }
        Ok(first)
    }

    fn judge(&mut self, at: &JsonPointer, role: RoleId, judge: &ir::JudgeRuntime) {
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
        // A slot over the limit is kept, so that the calls that name it resolve, and reported,
        // so that no parsed workflow holds it.
        let slots = (slots.iter())
            .map(|ir::Slot { name, max_bytes }| Slot {
                name: name.clone(),
                max_bytes: *max_bytes,
            })
            .collect();
        let judge = Judge {
            command: command.clone(),
            slots,
            result: result.clone(),
        };
        self.judges.insert(role, judge);
    }

    /// The judge grants of the agent at `agent` and its one submit grant, of its `grants`.
    fn grants(&mut self, agent: &JsonPointer, grants: &'ir [ir::Grant]) -> Grants {
        let list = agent.token("grants");
        let names = grants.iter().map(ir::Grant::name);
        (self.errors).unique(names, |index| place(&list, index, &grants[index]));
        let mut judge_grants = Vec::new();
        let mut submit_grants = Vec::new();
        for (index, grant) in grants.iter().enumerate() {
            let at = place(&list, index, grant);
            match grant {
                ir::Grant::Judge(grant) => {
                    judge_grants.push(self.judge_grant(&at, grant, &grants[..index]));
                }
                ir::Grant::Submit(ir::SubmitGrant { name }) => submit_grants.push((at, name)),
            }
        }
        // An absent list has no place in the IR's JSON, so its absence is the agent's.
        let at = if grants.is_empty() { agent } else { &list };
        let submit_grant = self.submit_grant(at, &submit_grants);
        (judge_grants.into_iter().collect(), submit_grant)
    }

    /// The role named `name`, which a reference at `at` names; reports an unknown name.
    fn resolve(&mut self, at: &JsonPointer, name: &Name) -> Result<RoleId, Reported> {
        let id = self.role_ids.get(name.as_str()).copied();
        id.ok_or_else(|| {
            let message = format!("no role is named `{name}`: name one listed in `roles`");
            self.errors.report(at.clone(), message)
        })
    }

    /// The kind of role `id` is, as a refusal names it, such as `a judge`.
    fn kind(&self, id: RoleId) -> &'static str {
        if self.judges.contains_key(&id) {
            "a judge"
        } else {
            "an agent"
        }
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

    /// The agent's one submit grant, of those named at their places in its grants. Reports
    /// none, at `at`, and each after the first.
    fn submit_grant(
        &mut self,
        at: &JsonPointer,
        grants: &[(JsonPointer, &Name)],
    ) -> Result<SubmitGrant, Reported> {
        let Some(((_, first), others)) = grants.split_first() else {
            return Err(self
                .errors
                .report(at.clone(), "no submit grant: give the agent exactly one"));
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
        let at = JsonPointer::root().token("score");
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
        let id = self.role_ids.get(role.as_str())?;
        if !self.judges.contains_key(id) {
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
        let id = self.resolve(&role_at, role)?;
        let Some(judge) = self.judges.get(&id) else {
            let kind = self.kind(id);
            return Err(self
                .errors
                .report(role_at, format!("`{role}` is {kind}: name a judge")));
        };
        let index = (judge.slots.iter()).position(|judge_slot| judge_slot.name == *slot);
        match index {
            Some(index) => Ok(SlotId {
                judge: JudgeId(id),
                index,
            }),
            None => Err(self.errors.report(
                at.token("slot"),
                format!("`{role}` has no slot named `{slot}`: name one of its slots"),
            )),
        }
    }
}

/// The place of `grant`, the `index`th of the list at `list`: its fields are below its kind, as in
/// `{"judge": {…}}`.
fn place(list: &JsonPointer, index: usize, grant: &ir::Grant) -> JsonPointer {
    let kind = match grant {
        ir::Grant::Judge(_) => "judge",
        ir::Grant::Submit(_) => "submit",
    };
    list.token(index).token(kind)
}
