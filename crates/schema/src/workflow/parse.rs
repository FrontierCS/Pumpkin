use std::collections::BTreeMap;
use std::time::Duration;

use super::errors::{Errors, Reported};
use super::{
    Agent, Holder, JobFunction, Judge, JudgeId, NowFunction, ParseErrors, RoleId, Runner, RunnerId,
    Sandbox, Score, Session, Slot, SlotId, Target, TargetId, Workflow,
};
use crate::ir::{self, JsonPointer, Name, RolePath};
use crate::sandbox::{MAX_OP_PAYLOAD_BYTES, MAX_TRANSFER_BYTES};

impl Workflow {
    /// Parses `ir`: resolves its names to ids, and reports each rule it breaks
    /// [at](super::ParseError::at) its place. The rules: one agent; names unique among roles,
    /// among a role's slots and among its grants; the agent's `dirs` and `files` give each path
    /// once, none below a file; each limit of bytes is within the protocol's; each grant names a
    /// role of its kind that no other grant of its kind names; an agent's runner has an image no
    /// judge or target has; every runner is leased, and every target called; the score names a
    /// judge's slot, and no other judge is listed. A rule about parts that fail to resolve goes
    /// unchecked, so fixing these errors may reveal others.
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
    targets: BTreeMap<RoleId, Target>,
    runners: BTreeMap<RoleId, Runner>,
    holders: Vec<Holding<'ir>>,
    /// Each job function's target, with the function's name, in the IR's order.
    called: Vec<(RoleId, &'ir Name)>,
}

/// A role that holds sessions, with each of them, its name and its place.
struct Holding<'ir> {
    role: RoleId,
    sessions: Vec<(JsonPointer, &'ir Name, &'ir ir::Session)>,
}

type Grants = (
    Result<Vec<JobFunction>, Reported>,
    Result<Vec<NowFunction>, Reported>,
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
        let agent = match self.first_agent() {
            Ok((at, agent, grants)) => {
                let list = at.token("grants");
                let names = grants.iter().map(|grant| &grant.name);
                (self.errors).unique(names, |index| list.token(index));
                let sessions = grants.iter().enumerate();
                let sessions = sessions.filter_map(|(index, grant)| match &grant.function {
                    ir::Function::Session(session) => {
                        Some((list.token(index).token("session"), &grant.name, session))
                    }
                    ir::Function::Job(_) | ir::Function::Now(_) => None,
                });
                // The agent's sessions come first, as [`Workflow::sessions`] says.
                let holding = Holding {
                    role: agent.role,
                    sessions: sessions.collect(),
                };
                self.holders.insert(0, holding);
                let parsed = self.grants(&at, grants);
                self.uncalled();
                Ok((agent, parsed))
            }
            Err(reported) => Err(reported),
        };
        let sessions = self.sessions(agent.as_ref().ok().map(|(agent, _)| agent.role));
        let score = self.score(score);
        let Self {
            errors,
            roles,
            targets,
            runners,
            ..
        } = self;
        let parsed = agent.and_then(|(agent, (job_functions, now_functions))| {
            let grants = (job_functions?, sessions?, now_functions?);
            Ok((agent, grants, score?))
        });
        let (agent, grants, (score, judge)) = errors.finish(parsed)?;
        let (job_functions, sessions, now_functions) = grants;
        Ok(Workflow {
            ir: ir.clone(),
            roles,
            agent,
            judge,
            targets,
            runners,
            job_functions,
            sessions,
            now_functions,
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
                ir::Runtime::Target(target) => self.target(&runtime_at.token("target"), id, target),
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

    /// Holds `sessions`, those of `role`'s kind at `at`, for [`Self::sessions`] to parse.
    fn hold(&mut self, at: &JsonPointer, role: RoleId, sessions: &'ir [ir::NamedSession]) {
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
    }

    fn target(&mut self, at: &JsonPointer, role: RoleId, target: &'ir ir::TargetRuntime) {
        let ir::TargetRuntime {
            cwd,
            env,
            egress,
            sessions,
        } = target;
        self.hold(at, role, sessions);
        let target = Target {
            cwd: cwd.clone(),
            env: env.clone(),
            model: egress.model.clone(),
        };
        self.targets.insert(role, target);
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
        self.hold(at, role, sessions);
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
        let mut job_functions = Vec::new();
        let mut now_functions = Vec::new();
        for (index, grant) in grants.iter().enumerate() {
            let at = place(&list, index, grant);
            match &grant.function {
                ir::Function::Job(job) => {
                    job_functions.push(self.job_function(&at, &grant.name, job));
                }
                ir::Function::Session(_) => {}
                ir::Function::Now(now) => {
                    now_functions.push(self.now_function(&at, &grant.name, now));
                }
            }
        }
        let job_functions = job_functions.into_iter().collect();
        (job_functions, now_functions.into_iter().collect())
    }

    fn job_function(
        &mut self,
        at: &JsonPointer,
        name: &'ir Name,
        job: &'ir ir::JobFunction,
    ) -> Result<JobFunction, Reported> {
        let ir::JobFunction {
            target,
            body,
            keywords,
            payload_max_bytes,
            calls,
            cumulative_seconds,
            call_timeout_seconds,
        } = job;
        // Every payload reaches its function through an op, which carries no more.
        let bounded = if payload_max_bytes.get() > MAX_OP_PAYLOAD_BYTES {
            Err(self.errors.report(
                at.token("payload_max_bytes"),
                format!(
                    "`{name}`'s `payload_max_bytes` is {payload_max_bytes}, over the op payload \
                     limit of {MAX_OP_PAYLOAD_BYTES}: give at most that"
                ),
            ))
        } else {
            Ok(())
        };
        let target_at = at.token("target");
        let id = self.resolve(&target_at, target)?;
        if !self.targets.contains_key(&id) {
            let kind = self.kind(id);
            let message = format!("`{target}` is {kind}: name a target as a job's target");
            return Err(self.errors.report(target_at, message));
        }
        // So that a target, like a judge, runs one call at a time.
        let shared = match self.called.iter().find(|(earlier, _)| *earlier == id) {
            Some((_, first)) => Err(self.errors.report(
                target_at,
                format!(
                    "`{target}` is the target of job functions `{first}` and `{name}`: name each \
                     target from one function"
                ),
            )),
            None => Ok(()),
        };
        self.called.push((id, name));
        shared.and(bounded).map(|()| JobFunction {
            name: name.clone(),
            target: TargetId(id),
            body: body.clone(),
            keywords: keywords.clone(),
            payload_max_bytes: *payload_max_bytes,
            calls: *calls,
            cumulative_seconds: *cumulative_seconds,
            call_timeout_seconds: *call_timeout_seconds,
        })
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
        // The agent, the judges and the targets alone hold sessions (`Parser::roles`).
        let holder = if agent == Some(holder) {
            Holder::Agent
        } else if self.judges.contains_key(&holder) {
            Holder::Judge(JudgeId(holder))
        } else {
            Holder::Target(TargetId(holder))
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
        // So that the agent never runs what a judge's or a target's image holds
        // ([`ir::Session::runner`]).
        let (role, image) = (&self.roles[runner.0].name, &self.roles[runner.0].image);
        let targets = self.targets.keys();
        let judge =
            (self.judges.keys().chain(targets)).find(|judge| self.roles[judge.0].image == *image);
        let Some(judge) = judge else {
            return Ok(());
        };
        let kind = if self.judges.contains_key(judge) {
            "judge"
        } else {
            "target"
        };
        let judge = &self.roles[judge.0].name;
        Err(self.errors.report(
            at.clone(),
            format!(
                "`{role}` runs {kind} `{judge}`'s image, {image}: give the runner an image no \
                 judge or target has"
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

    // So that each target is reached, as each runner is (`Self::unleased`).
    fn uncalled(&mut self) {
        let targets: Vec<RoleId> = self.targets.keys().copied().collect();
        for target in targets {
            if !self.called.iter().any(|(called, _)| *called == target) {
                let name = &self.roles[target.0].name;
                let message =
                    format!("`{name}` is a target that no job function names: name it from one");
                (self.errors).report(JsonPointer::root().token("roles").token(target.0), message);
            }
        }
    }

    // A judge runs only as the score's, so another would never run.
    fn unscored(&mut self, scorer: JudgeId) {
        let judges: Vec<RoleId> = self.judges.keys().copied().collect();
        for judge in judges.into_iter().filter(|&judge| judge != scorer.role()) {
            let name = &self.roles[judge.0].name;
            let message = format!(
                "`{name}` is a judge that the score doesn't name: name it as the score's role"
            );
            (self.errors).report(JsonPointer::root().token("roles").token(judge.0), message);
        }
    }

    fn kind(&self, id: RoleId) -> &'static str {
        if self.judges.contains_key(&id) {
            "a judge"
        } else if self.targets.contains_key(&id) {
            "a target"
        } else if self.runners.contains_key(&id) {
            "a runner"
        } else {
            "an agent"
        }
    }

    fn now_function(
        &mut self,
        at: &JsonPointer,
        name: &Name,
        now: &ir::NowFunction,
    ) -> Result<NowFunction, Reported> {
        let ir::NowFunction {
            body,
            payload_max_bytes,
        } = now;
        let max_bytes = payload_max_bytes.get();
        // A call's payload comes in an op, which carries no more.
        if max_bytes > MAX_OP_PAYLOAD_BYTES {
            return Err(self.errors.report(
                at.token("payload_max_bytes"),
                format!(
                    "`{name}`'s `payload_max_bytes` is {max_bytes}, over the op payload limit of \
                     {MAX_OP_PAYLOAD_BYTES}: give at most that"
                ),
            ));
        }
        Ok(NowFunction {
            name: name.clone(),
            body: body.clone(),
            payload_max_bytes: *payload_max_bytes,
        })
    }

    /// The score, and its judge, the workflow's one.
    fn score(&mut self, score: &ir::Score) -> Result<(Score, Judge), Reported> {
        let ir::Score {
            role,
            slot,
            timeout_seconds,
        } = score;
        let at = JsonPointer::root().token("score");
        let (slot, judge) = self.judge_slot(&at, role, slot)?;
        self.unscored(slot.judge);
        let score = Score {
            slot,
            timeout: Duration::from_secs(timeout_seconds.get()),
        };
        Ok((score, judge))
    }

    fn judge_slot(
        &mut self,
        at: &JsonPointer,
        role: &Name,
        slot: &Name,
    ) -> Result<(SlotId, Judge), Reported> {
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
            Some(index) => {
                let slot = SlotId {
                    judge: JudgeId(id),
                    index,
                };
                Ok((slot, judge.clone()))
            }
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
        ir::Function::Job(_) => "job",
        ir::Function::Session(_) => "session",
        ir::Function::Now(_) => "now",
    };
    list.token(index).token(kind)
}
