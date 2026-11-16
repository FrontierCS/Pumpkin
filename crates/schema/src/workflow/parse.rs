use std::collections::BTreeMap;
use std::time::Duration;

use super::errors::{Errors, Reported};
use super::{
    Agent, Holder, Judge, JudgeGrant, JudgeId, ParseErrors, RoleId, Runner, RunnerId, Sandbox,
    Score, Session, Slot, SlotId, SubmitGrant, Workflow,
};
use crate::ir::{self, JsonPointer, Name, RolePath};
use crate::sandbox::{MAX_OP_PAYLOAD_BYTES, MAX_TRANSFER_BYTES};

impl Workflow {
    /// Parses `ir`: resolves its names to ids, and reports each rule it breaks
    /// [at](super::ParseError::at) its place. The rules: one agent and one submit grant; names
    /// unique among roles, among a role's slots and among its grants; the agent's `dirs` and
    /// `files` give each path once, none below a file; a slot's `max_bytes` and a session's
    /// `transfer_bytes` are within the protocol's limits; each grant names a role of its kind that
    /// no other grant of its kind names, a judge grant a slot it has; an agent's runner has an
    /// image no judge has; every runner is leased; the score's judge is named by no judge grant. A
    /// rule about parts that fail to resolve goes unchecked, so fixing these errors may reveal
    /// others.
    pub fn parse(ir: &ir::Workflow) -> Result<Self, ParseErrors> {
        Parser::default().workflow(ir)
    }
}

#[derive(Default)]
struct Parser<'ir> {
    errors: Errors,
    role_ids: BTreeMap<&'ir str, RoleId>,
    roles: Vec<Sandbox>,
    agents: Vec<(JsonPointer, Agent, &'ir [ir::Grant])>,
    judges: BTreeMap<RoleId, Judge>,
    runners: BTreeMap<RoleId, Runner>,
    holders: Vec<Holding<'ir>>,
}

/// A role that holds sessions, with each of them, its name and its place.
struct Holding<'ir> {
    role: RoleId,
    sessions: Vec<(JsonPointer, &'ir Name, &'ir ir::Session)>,
}

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
            Ok((at, agent, grants)) => {
                let list = at.token("grants");
                let names = grants.iter().map(|grant| &grant.name);
                (self.errors).unique(names, |index| list.token(index));
                let sessions = grants.iter().enumerate();
                let sessions = sessions.filter_map(|(index, grant)| match &grant.function {
                    ir::Function::Session(session) => {
                        Some((list.token(index).token("session"), &grant.name, session))
                    }
                    ir::Function::Judge(_) | ir::Function::Submit(_) => None,
                });
                // The agent's sessions come first, as [`Workflow::sessions`] says.
                let holding = Holding {
                    role: agent.role,
                    sessions: sessions.collect(),
                };
                self.holders.insert(0, holding);
                (Ok((agent, self.grants(&at, grants))), grants)
            }
            Err(reported) => (Err(reported), &[][..]),
        };
        let sessions = self.sessions(agent.as_ref().ok().map(|(agent, _)| agent.role));
        let score = self.score(score, grants);
        let Self {
            errors,
            roles,
            judges,
            runners,
            ..
        } = self;
        let parsed = agent.and_then(|(agent, (judge_grants, submit_grant))| {
            Ok((agent, judge_grants?, sessions?, submit_grant?, score?))
        });
        let (agent, judge_grants, sessions, submit_grant, score) = errors.finish(parsed)?;
        Ok(Workflow {
            ir: ir.clone(),
            roles,
            agent,
            judges,
            runners,
            judge_grants,
            sessions,
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
            } = role;
            self.roles.push(Sandbox {
                name: name.clone(),
                image: image.clone(),
                user: *user,
                needs: needs.clone(),
                at: at.clone(),
            });
            let runtime_at = at.token("runtime");
            match runtime {
                ir::Runtime::Agent(agent) => self.agent(runtime_at.token("agent"), id, agent),
                ir::Runtime::Judge(judge) => self.judge(&runtime_at.token("judge"), id, judge),
                ir::Runtime::Runner(ir::RunnerRuntime { cwd, env }) => {
                    let runner = Runner {
                        cwd: cwd.clone(),
                        env: env.clone(),
                    };
                    self.runners.insert(id, runner);
                }
            }
            // A name given twice, which `unique` has reported, resolves to its first role.
            self.role_ids.entry(name.as_str()).or_insert(id);
        }
    }

    fn agent(&mut self, at: JsonPointer, role: RoleId, agent: &'ir ir::AgentRuntime) {
        let ir::AgentRuntime {
            command,
            dirs,
            files,
            stdin,
            deadline_seconds,
            finalize_grace_seconds,
            egress,
            grants,
        } = agent;
        self.agent_paths(&at, dirs, files);
        let model = egress.model.clone();
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
            model,
        };
        self.agents.push((at, agent, grants));
    }

    fn agent_paths(&mut self, runtime: &JsonPointer, dirs: &[ir::Dir], files: &[ir::File]) {
        let file_paths = files.iter().map(|file| &file.path);
        let all: Vec<&RolePath> = dirs.iter().map(|dir| &dir.path).chain(file_paths).collect();
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

    fn first_agent(&mut self) -> Result<(JsonPointer, Agent, &'ir [ir::Grant]), Reported> {
        let mut agents = std::mem::take(&mut self.agents).into_iter();
        let Some(first) = agents.next() else {
            return Err(self.errors.report(
                JsonPointer::root().token("roles"),
                "no role is an agent: a workflow has exactly one",
            ));
        };
        let name = |agent: &Agent| &self.roles[agent.role.0].name;
        for (_, other, _) in agents {
            let message = format!(
                "`{}` is a second agent, after `{}`: a workflow has exactly one",
                name(&other),
                name(&first.1)
            );
            let at = self.roles[other.role.0].at.token("runtime");
            self.errors.report(at, message);
        }
        Ok(first)
    }

    fn judge(&mut self, at: &JsonPointer, role: RoleId, judge: &'ir ir::JudgeRuntime) {
        let ir::JudgeRuntime {
            command,
            slots,
            result,
            egress,
            sessions,
        } = judge;
        let model = egress.model.clone();
        let list = at.token("sessions");
        let names = sessions.iter().map(|named| &named.name);
        self.errors.unique(names, |index| list.token(index));
        let sessions = sessions.iter().enumerate();
        let sessions = sessions.map(|(index, ir::NamedSession { name, session })| {
            (list.token(index).token("session"), name, session)
        });
        self.holders.push(Holding {
            role,
            sessions: sessions.collect(),
        });
        let list = at.token("slots");
        let names = slots.iter().map(|slot| &slot.name);
        self.errors.unique(names, |index| list.token(index));
        for (index, slot) in slots.iter().enumerate() {
            let max_bytes = slot.max_bytes.get();
            // Every payload reaches its judge through an op, which carries no more.
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
        // A slot over the limit is kept, so that the calls that name it resolve, and reported, so
        // that no parsed workflow holds it.
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
            model,
        };
        self.judges.insert(role, judge);
    }

    fn grants(&mut self, agent: &JsonPointer, grants: &'ir [ir::Grant]) -> Grants {
        let list = agent.token("grants");
        let mut judge_grants = Vec::new();
        let mut submit_grants = Vec::new();
        for (index, grant) in grants.iter().enumerate() {
            let at = place(&list, index, grant);
            match &grant.function {
                ir::Function::Judge(judge) => {
                    let earlier = &grants[..index];
                    judge_grants.push(self.judge_grant(&at, &grant.name, judge, earlier));
                }
                ir::Function::Session(_) => {}
                ir::Function::Submit(ir::SubmitGrant {}) => submit_grants.push((at, &grant.name)),
            }
        }
        // An absent list has no place in the IR's JSON, so its absence is the agent's.
        let at = if grants.is_empty() { agent } else { &list };
        let submit_grant = self.submit_grant(at, &submit_grants);
        (judge_grants.into_iter().collect(), submit_grant)
    }

    fn sessions(&mut self, agent: Option<RoleId>) -> Result<Vec<Session>, Reported> {
        let holders = std::mem::take(&mut self.holders);
        let mut parsed = Vec::new();
        let mut earlier: Vec<(RoleId, &Name, &ir::Session)> = Vec::new();
        for Holding { role, sessions } in &holders {
            for (at, name, session) in sessions {
                let held = (*role, *name, *session);
                let leased = self.session(at, held, &earlier, agent);
                parsed.push(leased);
                earlier.push(held);
            }
        }
        self.unleased(&earlier);
        parsed.into_iter().collect()
    }

    fn session(
        &mut self,
        at: &JsonPointer,
        (holder, name, grant): (RoleId, &Name, &ir::Session),
        earlier: &[(RoleId, &Name, &ir::Session)],
        agent: Option<RoleId>,
    ) -> Result<Session, Reported> {
        let ir::Session {
            runner,
            instances,
            instance_seconds,
            cumulative_seconds,
            commands,
            transfer_bytes,
        } = grant;
        // The host holds each transfer on its disk as it moves it.
        let bounded = if transfer_bytes.get() > MAX_TRANSFER_BYTES {
            Err(self.errors.report(
                at.token("transfer_bytes"),
                format!(
                    "`{name}`'s `transfer_bytes` is {transfer_bytes}, over the transfer limit of \
                     {MAX_TRANSFER_BYTES}: give at most that"
                ),
            ))
        } else {
            Ok(())
        };
        let runner_at = at.token("runner");
        let first = (earlier.iter()).find(|(_, _, earlier)| earlier.runner == *runner);
        let shared = match first {
            Some((first_holder, first, _)) => {
                let holder_name = |id: &RoleId| &self.roles[id.0].name;
                let message = format!(
                    "`{runner}` is named by `{}`'s session `{}` and `{}`'s `{name}`: name each \
                     runner from one session",
                    holder_name(first_holder),
                    first,
                    holder_name(&holder)
                );
                Err(self.errors.report(runner_at.clone(), message))
            }
            None => Ok(()),
        };
        // After the rules above, so that they are reported though the role fails to resolve.
        let id = self.resolve(&runner_at, runner)?;
        if !self.runners.contains_key(&id) {
            let kind = self.kind(id);
            return Err(self
                .errors
                .report(runner_at, format!("`{runner}` is {kind}: name a runner")));
        }
        if agent == Some(holder) {
            self.agents_runner(&runner_at, id)?;
        }
        // The agent and the judges alone hold grants that are parsed here (`Parser::roles`).
        let holder = if agent == Some(holder) {
            Holder::Agent
        } else {
            Holder::Judge(JudgeId(holder))
        };
        shared.and(bounded).map(|()| Session {
            name: name.clone(),
            holder,
            runner: RunnerId(id),
            instances: *instances,
            instance_seconds: *instance_seconds,
            cumulative_seconds: *cumulative_seconds,
            commands: *commands,
            transfer_bytes: *transfer_bytes,
        })
    }

    fn agents_runner(&mut self, at: &JsonPointer, runner: RoleId) -> Result<(), Reported> {
        // So that the agent never runs what a judge's image holds ([`ir::Session::runner`]).
        let (role, image) = (&self.roles[runner.0].name, &self.roles[runner.0].image);
        let judge = (self.judges.keys()).find(|judge| self.roles[judge.0].image == *image);
        let Some(judge) = judge else {
            return Ok(());
        };
        let judge = &self.roles[judge.0].name;
        Err(self.errors.report(
            at.clone(),
            format!(
                "`{role}` runs judge `{judge}`'s image, {image}: give the runner an image no \
                 judge has"
            ),
        ))
    }

    fn unleased(&mut self, grants: &[(RoleId, &Name, &ir::Session)]) {
        let named = |role: &Name| grants.iter().any(|(_, _, grant)| grant.runner == *role);
        let runners: Vec<RoleId> = self.runners.keys().copied().collect();
        for runner in runners {
            let name = &self.roles[runner.0].name;
            if !named(name) {
                let message =
                    format!("`{name}` is a runner that no session names: name it from one");
                (self.errors).report(JsonPointer::root().token("roles").token(runner.0), message);
            }
        }
    }

    fn resolve(&mut self, at: &JsonPointer, name: &Name) -> Result<RoleId, Reported> {
        let id = self.role_ids.get(name.as_str()).copied();
        id.ok_or_else(|| {
            let message = format!("no role is named `{name}`: name one listed in `roles`");
            self.errors.report(at.clone(), message)
        })
    }

    fn kind(&self, id: RoleId) -> &'static str {
        if self.judges.contains_key(&id) {
            "a judge"
        } else if self.runners.contains_key(&id) {
            "a runner"
        } else {
            "an agent"
        }
    }

    fn judge_grant(
        &mut self,
        at: &JsonPointer,
        name: &Name,
        grant: &ir::JudgeGrant,
        earlier: &[ir::Grant],
    ) -> Result<JudgeGrant, Reported> {
        let ir::JudgeGrant {
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
                    "`{role}` is named by judge grants `{first}` and `{name}`: name each judge \
                     from one grant"
                ),
            )),
            None => Ok(()),
        };
        // After the rule above, so that it is reported though the slot fails to resolve.
        let slot = self.judge_slot(at, role, slot)?;
        shared.map(|()| JudgeGrant {
            name: name.clone(),
            slot,
            calls: *calls,
            cumulative_seconds: *cumulative_seconds,
            call_timeout_seconds: *call_timeout_seconds,
        })
    }

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

    fn score(&mut self, score: &ir::Score, grants: &[ir::Grant]) -> Result<Score, Reported> {
        let ir::Score {
            role,
            slot,
            timeout_seconds,
        } = score;
        let at = JsonPointer::root().token("score");
        // So that the judge that scores the selection returns nothing to the agent.
        let shared = match self.judge_grant_naming(role, grants) {
            Some(grant) => Err(self.errors.report(
                at.token("role"),
                format!(
                    "`{role}` is also the role of judge grant `{grant}`: name a judge no grant names"
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

    fn judge_grant_naming<'g>(&self, role: &Name, grants: &'g [ir::Grant]) -> Option<&'g Name> {
        // The grants are compared by the role they name in the IR, so that a rule about the judges
        // they name holds even where a grant's slot or the score's fails to resolve.
        let id = self.role_ids.get(role.as_str())?;
        if !self.judges.contains_key(id) {
            return None;
        }
        grants.iter().find_map(|grant| match &grant.function {
            ir::Function::Judge(judge) => (judge.role == *role).then_some(&grant.name),
            ir::Function::Session(_) | ir::Function::Submit(_) => None,
        })
    }

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

fn place(list: &JsonPointer, index: usize, grant: &ir::Grant) -> JsonPointer {
    // Below the function's kind, the key that holds the function (`ir::Grant`'s written form).
    let kind = match grant.function {
        ir::Function::Judge(_) => "judge",
        ir::Function::Session(_) => "session",
        ir::Function::Submit(_) => "submit",
    };
    list.token(index).token(kind)
}
