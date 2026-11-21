//! Authorizing ops, and the replies to them.
//!
//! The core authorizes every op from the raw request ([`Request`]), and refuses it, with a
//! reason that says what to do instead, if it fails any of these:
//!
//! - The agent's ops are taken only during the work phase, and not once its work is ended; a
//!   call's, only while its body drives its target.
//! - An op names a grant its caller's role holds: a call one of the agent's job functions, a
//!   session's function one of its role's sessions, and a now function's call one of the agent's
//!   now functions.
//! - Its payload fits its function's `payload_max_bytes`; a job function's
//!   caller's arguments read as its body's parameters are typed.
//! - A job function has calls left, none of its calls queued or running, and call time left. A
//!   refused call is not charged.
//! - A session has fewer leases open than its `instances`, and time left, for its caller, and a
//!   lease asks to last at most its `instance_seconds`; a lease runs a command or a transfer once
//!   ready, one at a time, while its session has commands left, a command with a timeout, if
//!   given, of at most what the lease, and its holder's work, have left.
//! - A status names one of the caller's jobs, and a session's function one of the caller's leases
//!   of that session.
//!
//! A refused op changes nothing. An op that passes changes the core's state and calls for
//! [`Action`]s, which follow its reply: a call becomes a job, a lease, a command or a
//! transfer is started and metered by the `leases` module. Each reply gives the time left: the
//! agent's until its deadline, a call's until its timeout.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroU64;
use core::time::Duration;

use schema::ir::{Argv, Name};
use schema::sandbox::op::{
    CommandStatus, JobState, Last, LeaseRemaining, LeaseReply, LeaseState, LeaseStatusReply,
    OpReply, OpsReply, REASON_BYTES, SessionOp, StatusReply, TransferEnd, TransferStatus,
};
use schema::workflow::{AgentId, JobFunctionId, JobOf, NowFunctionId, RoleId, SessionId};

use super::jobs::{Job, State};
use super::leases::{self, Work};
use super::{Action, Caller, Core, Direction, JobId, LeaseId, OpId, PayloadRef, Phase, Request};

/// A caller whose ops the run takes now, with the role whose grants it holds, and when its work
/// ends, unless it ends it first, as a time since the run started.
pub(super) enum Working {
    /// The agent during its work, before the finalize grace.
    Agent { id: AgentId, deadline: Duration },
    /// A call, while its command runs, which its timeout ends at `end`.
    Job {
        job: JobId,
        role: RoleId,
        end: Duration,
    },
}

impl Working {
    pub(super) fn caller(&self) -> Caller {
        match self {
            Self::Agent { id, .. } => Caller::Agent(*id),
            Self::Job { job, .. } => Caller::Job(*job),
        }
    }

    fn role(&self) -> RoleId {
        match self {
            Self::Agent { id, .. } => id.role(),
            Self::Job { role, .. } => *role,
        }
    }

    pub(super) fn end(&self) -> Duration {
        match self {
            Self::Agent { deadline, .. } => *deadline,
            Self::Job { end, .. } => *end,
        }
    }
}

pub(super) enum Held {
    Job(JobFunctionId),
    Session(SessionId),
    Now(NowFunctionId),
}

impl Core<'_> {
    pub(super) fn op(
        &mut self,
        op: OpId,
        caller: Caller,
        request: Request,
        actions: &mut Vec<Action>,
    ) {
        let mut then = Vec::new();
        let reply =
            (self.working(caller)).and_then(|working| self.take(&working, request, &mut then));
        let reply = reply.unwrap_or_else(OpReply::refused);
        actions.push(Action::Reply { op, reply });
        actions.append(&mut then);
    }

    /// `caller`, if it works: the agent in the work phase, a call while it runs. Else
    /// why the run takes no more ops of `caller`'s.
    pub(super) fn working(&self, caller: Caller) -> Result<Working, String> {
        match (&self.phase, caller) {
            (Phase::Abandoned, _) => {
                Err("the run has passed its hard end: make no more ops".to_owned())
            }
            (_, Caller::Job(job)) => match self.jobs.get(job) {
                Some(Job {
                    call,
                    timeout,
                    state: State::Running { started },
                }) => {
                    let body_end = started.saturating_add(*timeout);
                    let (role, end) = match *call {
                        JobOf::Function(function) => {
                            let function = self.workflow.job_function(function);
                            (
                                function.target.role(),
                                self.agent_due(function.holder).min(body_end),
                            )
                        }
                        JobOf::Final => (self.workflow.final_call().target.role(), body_end),
                    };
                    Ok(Working::Job { job, role, end })
                }
                Some(Job {
                    state: State::Queued | State::Ending { .. } | State::Ended { .. },
                    ..
                })
                | None => Err("the call has ended: make no more ops".to_owned()),
            },
            (_, Caller::Agent(id)) => {
                let Some(work) = self.agents.get(&id) else {
                    return Err("this agent is outside the workflow: make no ops".to_owned());
                };
                if work.ended.is_some() || work.grace_end.is_some() {
                    Err("your work has ended: make no more ops".to_owned())
                } else {
                    Ok(Working::Agent {
                        id,
                        deadline: self.phase_due(),
                    })
                }
            }
        }
    }

    fn take(
        &mut self,
        working: &Working,
        request: Request,
        then: &mut Vec<Action>,
    ) -> Result<OpReply, String> {
        let caller = working.caller();
        match request {
            Request::Ops => Ok(OpReply::Ops(self.ops(working))),
            Request::Status { job } => self.own_job(caller, job).map(OpReply::Status),
            Request::Call {
                function,
                payload,
                arguments,
            } => self.call(working, &function, payload, &arguments, then),
            Request::Now {
                function,
                payload,
                evaluated,
            } => self.now(working, &function, &payload, evaluated, then),
            Request::Session { grant, op } => self.session(working, &grant, op, then),
        }
    }

    /// The reply to `op`, a function of the session named `grant`, by `working`, and the actions it
    /// takes.
    fn session(
        &mut self,
        working: &Working,
        grant: &Name,
        op: SessionOp,
        then: &mut Vec<Action>,
    ) -> Result<OpReply, String> {
        let id = match self.held(working, grant)? {
            Held::Session(id) => id,
            held @ (Held::Job(..) | Held::Now(..)) => {
                return Err(held.misnamed(grant, "a session"));
            }
        };
        let (caller, end) = (working.caller(), working.end());
        match op {
            SessionOp::Open { instance_seconds } => {
                let opened = self.lease_open((caller, id), end, instance_seconds, then);
                opened.map(OpReply::Lease)
            }
            SessionOp::Run {
                lease,
                argv,
                timeout_seconds,
            } => {
                let ran = self.lease_run((caller, id, lease), end, argv, timeout_seconds, then);
                ran.map(OpReply::Lease)
            }
            SessionOp::Put {
                lease,
                tree,
                to,
                replace,
            } => {
                let direction = Direction::Put { tree, to, replace };
                (self.transfer((caller, id, lease), direction, then)).map(OpReply::Lease)
            }
            SessionOp::Get { lease, from, to } => {
                let direction = Direction::Get { from, to };
                (self.transfer((caller, id, lease), direction, then)).map(OpReply::Lease)
            }
            SessionOp::Status { lease } => {
                let lease = self.own_lease((caller, id), lease)?;
                Ok(OpReply::LeaseStatus(self.lease_status(lease)))
            }
            SessionOp::Close { lease } => {
                let lease = self.own_lease((caller, id), lease)?;
                if self.leases[lease].close(self.now, leases::End::Closed) {
                    then.push(Action::CloseLease { lease });
                }
                Ok(OpReply::Lease(self.lease_reply(lease)))
            }
        }
    }

    // A lease that another caller holds is refused as one that doesn't exist.
    fn own_lease(
        &self,
        (caller, session): (Caller, SessionId),
        number: u64,
    ) -> Result<LeaseId, String> {
        let name = &self.workflow.session(session).name;
        let lease = self.leases.find(number);
        let lease =
            (lease.filter(|lease| self.leases[*lease].holder == caller)).ok_or_else(|| {
                format!("you have no lease {number}: name one that `{name}.open` returned")
            })?;
        let of = self.leases[lease].grant;
        if of != session {
            let of = &self.workflow.session(of).name;
            return Err(format!(
                "lease {number} is `{of}`'s, not `{name}`'s: call `{of}`'s functions for it"
            ));
        }
        Ok(lease)
    }

    fn own_job(&self, caller: Caller, number: u64) -> Result<StatusReply, String> {
        // The agent asks only about its calls to job functions: any job it didn't make is refused
        // as absent.
        let job = JobId(number);
        if let Some(found) = self.jobs.get(job)
            && let JobOf::Function(function) = found.call
            && caller == Caller::Agent(self.workflow.job_function(function).holder)
        {
            return Ok(self.status(job, found, function));
        }
        Err(format!(
            "you have no job {number}: ask about a job that one of your calls returned"
        ))
    }

    /// The time from now until `end`, as a reply gives the time left: in whole seconds rounded
    /// down, and 0 once it has passed.
    pub(super) fn seconds_left(&self, end: Duration) -> u64 {
        end.saturating_sub(self.now).as_secs()
    }

    pub(super) fn held(&self, working: &Working, name: &Name) -> Result<Held, String> {
        let role = working.role();
        let job = (self.workflow.job_functions_of(role))
            .find(|(_, function)| function.name == *name)
            .map(|(id, _)| Held::Job(id));
        let lease = (self.workflow.sessions())
            .find(|(_, grant)| grant.holder.role() == role && grant.name == *name)
            .map(|(id, _)| Held::Session(id));
        let now = (self.workflow.now_functions_of(role))
            .find(|(_, function)| function.name == *name)
            .map(|(id, _)| Held::Now(id));
        let held = (job.or(lease)).or(now);
        held.ok_or_else(|| format!("you hold no grant named `{name}`: call one that `ops` lists"))
    }

    /// Opens a lease of session `id` for `caller`, whose work ends at `end`, by which the lease's
    /// runner is to have started.
    fn lease_open(
        &mut self,
        (caller, id): (Caller, SessionId),
        end: Duration,
        instance_seconds: Option<NonZeroU64>,
        then: &mut Vec<Action>,
    ) -> Result<LeaseReply, String> {
        let grant = self.workflow.session(id);
        let (name, usage) = (&grant.name, self.leases.usage(id, caller, self.now));
        let most = grant.instance_seconds;
        if let Some(seconds) = instance_seconds
            && seconds > most
        {
            return Err(format!(
                "the time of {seconds} s is longer than `{name}` lets a lease last, {most} s: \
                 open it again as `sandboxctl call {name}.open --seconds {most}`"
            ));
        }
        let life = instance_seconds.map_or(grant.instance(), |seconds| {
            Duration::from_secs(seconds.get())
        });
        let instances = grant.instances;
        if usage.open >= instances.get() {
            let closing = match usage.closing {
                0 => String::new(),
                closing => format!(
                    ", {closing} of them `closing`, which holds its instance until its runner is \
                     gone"
                ),
            };
            return Err(format!(
                "`{name}` has {instances} leases open, as many as it allows at once{closing}: open \
                 another once `{name}.status` says one has ended"
            ));
        }
        if let Some(total) = grant.cumulative()
            && usage.used >= total
        {
            return Err(format!(
                "`{name}` has no time left of the {} s it allows: open no more leases of it",
                total.as_secs()
            ));
        }
        let lease = self.leases.open((caller, id), life, self.now);
        let instance = self.leases[lease].instance;
        then.push(Action::StartLease {
            lease,
            holder: caller,
            grant: id,
            instance,
            start_by: end,
        });
        Ok(self.lease_reply(lease))
    }

    /// Checks that lease `number` of `session` can take work now, and returns it with when its
    /// runner became ready: the caller holds it, its runner is ready and running nothing, and its
    /// session has commands left. `act` ends the advice in a refusal.
    fn idle_lease(
        &self,
        (caller, session, number): (Caller, SessionId, u64),
        act: &str,
    ) -> Result<(LeaseId, Duration), String> {
        let id = self.own_lease((caller, session), number)?;
        let name = &self.workflow.session(session).name;
        let lease = &self.leases[id];
        let since = match lease.state {
            leases::State::Starting => {
                return Err(format!(
                    "lease {number} is still starting: {act} once `{name}.status` says it is ready"
                ));
            }
            leases::State::Ready { since } => since,
            leases::State::Closing { .. } | leases::State::Ended { .. } => {
                return Err(format!("lease {number} has ended: open another"));
            }
        };
        if let Some(running) = lease.running() {
            return Err(format!(
                "lease {number} runs {}, one at a time: {act} once `{name}.status` says it has \
                 ended",
                running.name()
            ));
        }
        let grant = self.workflow.session(lease.grant);
        let usage = self.leases.usage(lease.grant, caller, self.now);
        if let Some(commands) = grant.commands
            && usage.commands >= commands.get()
        {
            return Err(format!(
                "`{}` has no commands left of the {commands} it allows, a transfer counting as \
                 one: run and move no more",
                grant.name
            ));
        }
        Ok((id, since))
    }

    fn transfer(
        &mut self,
        (caller, session, number): (Caller, SessionId, u64),
        direction: Direction,
        then: &mut Vec<Action>,
    ) -> Result<LeaseReply, String> {
        let act = match direction {
            Direction::Put { .. } => "put a tree",
            Direction::Get { .. } => "get a tree",
        };
        let (id, _) = self.idle_lease((caller, session, number), act)?;
        let work = |number| Work::Transfer {
            number,
            ended: None,
        };
        let transfer = self.leases[id].accept(work, self.now);
        then.push(Action::Transfer {
            lease: id,
            holder: caller,
            transfer,
            direction,
            max_bytes: self.workflow.session(self.leases[id].grant).transfer_bytes,
        });
        Ok(self.lease_reply(id))
    }

    fn lease_run(
        &mut self,
        (caller, session, number): (Caller, SessionId, u64),
        work_end: Duration,
        argv: Argv,
        timeout_seconds: Option<NonZeroU64>,
        then: &mut Vec<Action>,
    ) -> Result<LeaseReply, String> {
        let (id, since) = self.idle_lease((caller, session, number), "run a command")?;
        let lease = &self.leases[id];
        let timeout = timeout_seconds.map(|seconds| Duration::from_secs(seconds.get()));
        // The lease ends by its own time, and by its holder's work's end.
        let end = self.ready_end(lease, since).min(work_end);
        let left = self.seconds_left(end);
        if let Some(timeout) = timeout_seconds
            && timeout.get() > left
        {
            let name = &self.workflow.session(session).name;
            let rerun = rerun((name, number), NonZeroU64::new(left), &argv);
            return Err(format!(
                "the timeout of {timeout} s is longer than lease {number} has left, {left} s: \
                 run it again as `{rerun}`"
            ));
        }
        let work = |number| Work::Command {
            number,
            ended: None,
        };
        let command = self.leases[id].accept(work, self.now);
        then.push(Action::RunCommand {
            lease: id,
            command,
            argv,
            timeout,
        });
        Ok(self.lease_reply(id))
    }

    fn ops(&self, working: &Working) -> OpsReply {
        let (caller, role, end) = (working.caller(), working.role(), working.end());
        self.workflow
            .ops(role, self.seconds_left(end), |id, grant| {
                self.grant_remaining((id, caller), grant.instance())
            })
    }

    fn status(&self, id: JobId, job: &Job, function: JobFunctionId) -> StatusReply {
        let state = match &job.state {
            State::Queued => JobState::Queued,
            State::Running { .. } | State::Ending { .. } => JobState::Running,
            // A call has ended only once every lease it held has.
            State::Ended { .. } if self.leases.holds(Caller::Job(id)) => JobState::Running,
            State::Ended {
                outcome: Ok(result),
                model_requests_over_budget,
                ..
            } => result.job_state(*model_requests_over_budget),
            State::Ended {
                outcome: Err(failure),
                model_requests_over_budget,
                ..
            } => JobState::Errored {
                failure: failure.reply(),
                model_requests_over_budget: *model_requests_over_budget,
            },
        };
        StatusReply {
            job: id.0,
            state,
            remaining: self.remaining(function),
        }
    }

    fn lease_reply(&self, id: LeaseId) -> LeaseReply {
        LeaseReply {
            lease: id.0,
            remaining: self.lease_remaining(id),
        }
    }

    /// The recorded state of an admitted lease, available only to its holder.
    pub fn held_lease(&self, holder: Caller, lease: LeaseId) -> Option<LeaseStatusReply> {
        let id = self.leases.find(lease.0)?;
        (self.leases[id].holder == holder).then(|| self.lease_status(id))
    }

    fn lease_status(&self, id: LeaseId) -> LeaseStatusReply {
        let lease = &self.leases[id];
        let (state, failure) = match &lease.state {
            leases::State::Starting => (LeaseState::Starting, None),
            leases::State::Ready { .. } if lease.running().is_some() => (LeaseState::Running, None),
            leases::State::Ready { .. } => (LeaseState::Ready, None),
            leases::State::Closing { .. } => (LeaseState::Closing, None),
            leases::State::Ended { end, .. } => match end {
                leases::End::Closed => (LeaseState::Closed, None),
                leases::End::Expired => (LeaseState::Expired, None),
                leases::End::Failed(reason) => (LeaseState::Failed, Some(cut(reason))),
            },
        };
        let last = lease.last.as_ref().map(|work| match work {
            Work::Command { number, ended } => {
                let (end, stdout, stderr) = match ended {
                    Some((end, stdout, stderr)) => (Some(*end), stdout.clone(), stderr.clone()),
                    None => (None, String::new(), String::new()),
                };
                Last::Command(CommandStatus {
                    command: *number,
                    end,
                    stdout,
                    stderr,
                })
            }
            Work::Transfer { number, ended } => Last::Transfer(TransferStatus {
                transfer: *number,
                end: ended.as_ref().map(|end| match end {
                    TransferEnd::Moved { entries, bytes } => TransferEnd::Moved {
                        entries: *entries,
                        bytes: *bytes,
                    },
                    TransferEnd::Failed(reason) => TransferEnd::Failed(cut(reason)),
                }),
            }),
        });
        let (busy, idle) = lease.busy_idle(self.now);
        LeaseStatusReply {
            lease: id.0,
            state,
            elapsed_seconds: self.now.saturating_sub(lease.opened).as_secs(),
            busy_seconds: busy.as_secs(),
            idle_seconds: idle.as_secs(),
            failure,
            last,
            remaining: self.lease_remaining(id),
        }
    }

    fn lease_remaining(&self, id: LeaseId) -> LeaseRemaining {
        let lease = &self.leases[id];
        let own = match lease.state {
            // A lease's life counts from when its runner is ready.
            leases::State::Starting => lease.life,
            leases::State::Ready { since } => {
                (lease.life).saturating_sub(self.now.saturating_sub(since))
            }
            leases::State::Closing { .. } | leases::State::Ended { .. } => Duration::ZERO,
        };
        self.grant_remaining((lease.grant, lease.holder), own)
    }

    /// What grant `id` has left of its limits for `holder`, with `own` as a lease's own time left.
    fn grant_remaining(&self, (id, holder): (SessionId, Caller), own: Duration) -> LeaseRemaining {
        let grant = self.workflow.session(id);
        let usage = self.leases.usage(id, holder, self.now);
        let cumulative = grant.cumulative();
        LeaseRemaining {
            instances: grant.instances.get().saturating_sub(usage.open),
            instance_seconds: own.as_secs(),
            cumulative_seconds: cumulative.map(|total| total.saturating_sub(usage.used).as_secs()),
            commands: (grant.commands)
                .map(|commands| commands.get().saturating_sub(usage.commands)),
        }
    }
}

impl Held {
    pub(super) fn misnamed(&self, name: &Name, kind: &str) -> String {
        let (is, instead) = match self {
            Self::Job(..) => ("a job function", "call it".to_owned()),
            Self::Now(..) => ("a now function", "call it".to_owned()),
            Self::Session(_) => (
                "a session",
                format!("call its functions, as `{name}.open`,"),
            ),
        };
        format!("`{name}` is {is}, not {kind}: {instead} instead")
    }
}

// A runner's failure, as a lease's status gives it: the start of `text`, of at most
// `REASON_BYTES`.
fn cut(text: &str) -> String {
    let mut end = REASON_BYTES.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

// A refusal offers this line to be pasted back, so each word is quoted for a POSIX shell.
fn rerun((session, lease): (&Name, u64), timeout: Option<NonZeroU64>, argv: &Argv) -> String {
    let quoted = |arg: &str| {
        let plain = |byte: u8| byte.is_ascii_alphanumeric() || b"-_./=:@%+,".contains(&byte);
        if !arg.is_empty() && arg.bytes().all(plain) {
            arg.to_owned()
        } else {
            format!("'{}'", arg.replace('\'', r"'\''"))
        }
    };
    let timeout = timeout.map_or(String::new(), |seconds| format!(" --timeout {seconds}"));
    let words = core::iter::once(argv.program()).chain(argv.args().iter().map(String::as_str));
    let words: Vec<String> = words.map(quoted).collect();
    format!(
        "sandboxctl call {session}.run {lease}{timeout} -- {}",
        words.join(" ")
    )
}

// Refuses a payload over `max_bytes`.
pub(crate) fn fits(name: &Name, payload: &PayloadRef, max_bytes: NonZeroU64) -> Result<(), String> {
    if payload.size <= max_bytes.get() {
        Ok(())
    } else {
        Err(format!(
            "the payload is {} bytes, over `{name}`'s limit of {max_bytes}: send a smaller one",
            payload.size
        ))
    }
}
