use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::time::Duration;

use super::errors::{Errors, Reported};
use super::{
    Agent, AgentId, Final, Holder, JobFunction, NowFunction, ParseErrors, RoleId, Runner, RunnerId,
    Sandbox, Session, Target, TargetId, Workflow,
};
use crate::ir::{self, JsonPointer, Name, RolePath};
use crate::sandbox::{MAX_OP_PAYLOAD_BYTES, MAX_TRANSFER_BYTES};

impl Workflow {
    /// Parses `ir`: resolves its names to ids, and reports each rule it breaks
    /// [at](super::ParseError::at) its place. The rules: one agent; names unique among roles and
    /// among a role's grants; the agent's `dirs` and `files` give each path once, none below a
    /// file; each limit of bytes is within the protocol's; each grant names a role of its kind
    /// that no other grant of its kind names; an agent's runner has an image no target has; the
    /// final call's target is one that no job function names; every runner is leased, and every
    /// target called. A rule about parts that fail to resolve goes
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
    targets: BTreeMap<RoleId, Target>,
    runners: BTreeMap<RoleId, Runner>,
    holders: Vec<Holding<'ir>>,
    /// Each job function's target, with the function's name, in the IR's order.
    called: Vec<(RoleId, &'ir Name)>,
}

/// A role that holds sessions, with each of them, its name and its place.
struct Holding<'ir> {
    holder: Holder,
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
            final_call,
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
                    holder: Holder::Agent(agent.id),
                    sessions: sessions.collect(),
                };
                self.holders.insert(0, holding);
                let grants = self.grants(&at, agent.id, grants);
                Ok((agent, grants))
            }
            Err(reported) => Err(reported),
        };
        // After the grants, since the final call's target is one that no job function names.
        let target = self.final_target(&final_call.target);
        // With no agent, it holds no grants, so no target is checked against job functions.
        if agent.is_ok() {
            self.uncalled(target.as_ref().ok().copied());
        }
        let final_call = self.final_call(final_call, target);
        let sessions = self.sessions();
        let Self {
            errors,
            roles,
            targets,
            runners,
            ..
        } = self;
        let parsed = agent.and_then(|(agent, (job_functions, now_functions))| {
            let grants = (job_functions?, sessions?, now_functions?);
            Ok((agent, grants, final_call?))
        });
        let (agent, grants, final_call) = errors.finish(parsed)?;
        let (job_functions, sessions, now_functions) = grants;
        Ok(Workflow {
            ir: ir.clone(),
            roles,
            agent,
            targets,
            runners,
            job_functions,
            sessions,
            now_functions,
            final_call,
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
            id: AgentId(role),
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
        let name = |agent: &Agent| &self.roles[agent.id.role().0].name;
        for (_, other, _) in agents {
            let message = format!(
                "`{}` is a second agent, after `{}`: a workflow has exactly one",
                name(&other),
                name(&first.1)
            );
            let at = self.roles[other.id.role().0].at.token("runtime");
            self.errors.report(at, message);
        }
        Ok(first)
    }

    /// Holds `sessions`, target `target`'s at `at`, for [`Self::sessions`] to parse.
    fn hold(&mut self, at: &JsonPointer, target: TargetId, sessions: &'ir [ir::NamedSession]) {
        let list = at.token("sessions");
        let names = sessions.iter().map(|named| &named.name);
        self.errors.unique(names, |index| list.token(index));
        let sessions = sessions.iter().enumerate();
        let sessions = sessions.map(|(index, ir::NamedSession { name, session })| {
            (list.token(index).token("session"), name, session)
        });
        self.holders.push(Holding {
            holder: Holder::Target(target),
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
        self.hold(at, TargetId(role), sessions);
        let target = Target {
            cwd: cwd.clone(),
            env: env.clone(),
            model: egress.model.clone(),
        };
        self.targets.insert(role, target);
    }

    fn grants(&mut self, at: &JsonPointer, holder: AgentId, grants: &'ir [ir::Grant]) -> Grants {
        let list = at.token("grants");
        let mut job_functions = Vec::new();
        let mut now_functions = Vec::new();
        for (index, grant) in grants.iter().enumerate() {
            let at = place(&list, index, grant);
            match &grant.function {
                ir::Function::Job(job) => {
                    job_functions.push(self.job_function(&at, holder, &grant.name, job));
                }
                ir::Function::Session(_) => {}
                ir::Function::Now(now) => {
                    now_functions.push(self.now_function(&at, holder, &grant.name, now));
                }
            }
        }
        let job_functions = job_functions.into_iter().collect();
        (job_functions, now_functions.into_iter().collect())
    }

    fn job_function(
        &mut self,
        at: &JsonPointer,
        holder: AgentId,
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
        let bounded = self.payload_bound(at, &format!("`{name}`'s"), *payload_max_bytes);
        let target_at = at.token("target");
        let target_id = self.call_target(&target_at, target, "a job's target")?;
        // So that a target runs one call at a time, as its function does.
        let shared = match self.caller(target_id) {
            Some(first) => Err(self.errors.report(
                target_at,
                format!(
                    "`{target}` is the target of job functions `{first}` and `{name}`: name each \
                     target from one function"
                ),
            )),
            None => Ok(()),
        };
        self.called.push((target_id.role(), name));
        shared.and(bounded).map(|payload_max_bytes| JobFunction {
            holder,
            name: name.clone(),
            target: target_id,
            body: body.clone(),
            keywords: keywords.clone(),
            payload_max_bytes,
            calls: *calls,
            cumulative_seconds: *cumulative_seconds,
            call_timeout_seconds: *call_timeout_seconds,
        })
    }

    fn sessions(&mut self) -> Result<Vec<Session>, Reported> {
        let holders = std::mem::take(&mut self.holders);
        let mut parsed = Vec::new();
        let mut earlier: Vec<(Holder, &Name, &ir::Session)> = Vec::new();
        for Holding { holder, sessions } in &holders {
            for (at, name, session) in sessions {
                let held = (*holder, *name, *session);
                let leased = self.session(at, held, &earlier);
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
        (holder, name, grant): (Holder, &Name, &ir::Session),
        earlier: &[(Holder, &Name, &ir::Session)],
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
                let holder_name = |held: &Holder| &self.roles[held.role().0].name;
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
        if let Holder::Agent(_) = holder {
            self.agents_runner(&runner_at, id)?;
        }
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
        // So that the agent never runs what a target's image holds ([`ir::Session::runner`]).
        let (role, image) = (&self.roles[runner.0].name, &self.roles[runner.0].image);
        let mut targets = self.targets.keys();
        let Some(target) = targets.find(|target| self.roles[target.0].image == *image) else {
            return Ok(());
        };
        let target = &self.roles[target.0].name;
        Err(self.errors.report(
            at.clone(),
            format!(
                "`{role}` runs target `{target}`'s image, {image}: give the runner an image no \
                 target has"
            ),
        ))
    }

    fn unleased(&mut self, grants: &[(Holder, &Name, &ir::Session)]) {
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
    fn uncalled(&mut self, final_target: Option<TargetId>) {
        let targets: Vec<RoleId> = self.targets.keys().copied().collect();
        for target in targets {
            let called = self.called.iter().any(|(called, _)| *called == target);
            if !called && final_target != Some(TargetId(target)) {
                let name = &self.roles[target.0].name;
                let message = format!(
                    "`{name}` is a target that no job function or final call names: name it from \
                     one"
                );
                (self.errors).report(JsonPointer::root().token("roles").token(target.0), message);
            }
        }
    }

    fn kind(&self, id: RoleId) -> &'static str {
        if self.targets.contains_key(&id) {
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
        holder: AgentId,
        name: &Name,
        now: &ir::NowFunction,
    ) -> Result<NowFunction, Reported> {
        let ir::NowFunction {
            body,
            payload_max_bytes,
        } = now;
        let payload_max_bytes =
            self.payload_bound(at, &format!("`{name}`'s"), *payload_max_bytes)?;
        Ok(NowFunction {
            holder,
            name: name.clone(),
            body: body.clone(),
            payload_max_bytes,
        })
    }

    /// `bytes`, the `payload_max_bytes` below `at` of `whose` (as "`train`'s"), if an op can
    /// carry that many.
    fn payload_bound(
        &mut self,
        at: &JsonPointer,
        whose: &str,
        bytes: NonZeroU64,
    ) -> Result<NonZeroU64, Reported> {
        // Every payload reaches its body through an op, which carries no more.
        if bytes.get() > MAX_OP_PAYLOAD_BYTES {
            return Err(self.errors.report(
                at.token("payload_max_bytes"),
                format!(
                    "{whose} `payload_max_bytes` is {bytes}, over the op payload limit of \
                     {MAX_OP_PAYLOAD_BYTES}: give at most that"
                ),
            ));
        }
        Ok(bytes)
    }

    /// The target that `target`, at `at`, names as `named_as` (as "a job's target").
    fn call_target(
        &mut self,
        at: &JsonPointer,
        target: &Name,
        named_as: &str,
    ) -> Result<TargetId, Reported> {
        let id = self.resolve(at, target)?;
        if !self.targets.contains_key(&id) {
            let kind = self.kind(id);
            let message = format!("`{target}` is {kind}: name a target as {named_as}");
            return Err(self.errors.report(at.clone(), message));
        }
        Ok(TargetId(id))
    }

    /// The job function whose target is `target`, if one's is.
    fn caller(&self, target: TargetId) -> Option<&'ir Name> {
        let mut called = self.called.iter();
        called
            .find(|(called, _)| *called == target.role())
            .map(|(_, name)| *name)
    }

    fn final_target(&mut self, target: &Name) -> Result<TargetId, Reported> {
        let at = JsonPointer::root().token("final").token("target");
        let id = self.call_target(&at, target, "the final call's target")?;
        // So that the final call, like a job, has its target to itself.
        if let Some(function) = self.caller(id) {
            let message = format!(
                "`{target}` is the final call's target and `{function}`'s: give the final call a \
                 role no job names"
            );
            return Err(self.errors.report(at, message));
        }
        Ok(id)
    }

    fn final_call(
        &mut self,
        final_call: &ir::Final,
        target: Result<TargetId, Reported>,
    ) -> Result<Final, Reported> {
        let ir::Final {
            target: _,
            body,
            keywords,
            payload_max_bytes,
            timeout_seconds,
        } = final_call;
        let at = JsonPointer::root().token("final");
        let payload_max_bytes = self.payload_bound(&at, "the final call's", *payload_max_bytes);
        let target = target?;
        payload_max_bytes.map(|payload_max_bytes| Final {
            target,
            body: body.clone(),
            keywords: keywords.clone(),
            payload_max_bytes,
            timeout: Duration::from_secs(timeout_seconds.get()),
        })
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
