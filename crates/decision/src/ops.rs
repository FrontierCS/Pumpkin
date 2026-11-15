//! Authorizing ops, and the replies to them.
//!
//! The core authorizes every op from the raw request ([`Request`]) and refuses, with a reason,
//! one that fails any of these:
//!
//! - The agent's ops are taken only during the work phase, and refused once a finalize is
//!   accepted ([`finalize_grace_seconds`](schema::ir::AgentRuntime::finalize_grace_seconds)); a
//!   call's, only while its judge's command runs.
//! - An op names a grant its caller's role holds: a judge call one of the agent's judge grants, a
//!   lease one of its role's lease grants, and a submission the agent's submit grant.
//! - Its payload isn't empty, and fits its slot's
//!   [`max_bytes`](schema::ir::Slot::max_bytes), the score's for a submission.
//! - A judge grant has calls left, none of its calls queued or running, and judge time left.
//!   A refused call is not charged.
//! - A lease grant has fewer leases open than its `instances`, and time left, for its caller, and
//!   a lease asks to last at most its `instance_seconds`; a lease runs a command or a transfer
//!   once ready, one at a time, while its grant has commands left, a command with a timeout, if
//!   given, of at most what the lease, and its holder's work, have left.
//! - A status names one of the caller's jobs, and a lease op one of its leases.
//!
//! A judge call that passes becomes a job, run in the background with the timeout that
//! [`call_timeout_seconds`](schema::ir::JudgeGrant::call_timeout_seconds) describes; a lease, a
//! command or a transfer runs likewise, as the `leases` module meters it, a transfer within its
//! grant's [`transfer_bytes`](schema::ir::LeaseGrant::transfer_bytes). Each reply gives the time
//! left: the agent's until its deadline, a call's until its timeout.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroU64;
use core::time::Duration;

use schema::ir::{Argv, Name};
use schema::sandbox::op::{
    self, CommandStatus, Grant, JobState, JudgeReply, Last, LeaseRemaining, LeaseReply, LeaseState,
    LeaseStatusReply, OpReply, OpsReply, REASON_BYTES, Remaining, StatusReply, SubmitReply,
    SubmitState, TransferEnd, TransferStatus,
};
use schema::workflow::{JudgeCall, JudgeGrant, JudgeGrantId, LeaseGrant, LeaseGrantId, RoleId};

use super::jobs::{Job, State};
use super::leases::{self, Work};
use super::{
    Action, Caller, Core, Direction, JobId, LeaseId, OpId, PayloadRef, Phase, Request, WorkEnd,
};

/// A caller whose ops the run takes now ([`Core::working`]), with the role whose grants it
/// holds, and when its work ends, unless it ends it first, as a time since the run started.
#[derive(Clone, Copy)]
pub(super) enum Working {
    /// The agent, in the work phase, which ends at the deadline.
    Agent { role: RoleId, deadline: Duration },
    /// A call, while its command runs, which its timeout ends at `end`.
    Job {
        job: JobId,
        role: RoleId,
        end: Duration,
    },
}

impl Working {
    fn caller(self) -> Caller {
        match self {
            Self::Agent { .. } => Caller::Agent,
            Self::Job { job, .. } => Caller::Job(job),
        }
    }

    fn role(self) -> RoleId {
        match self {
            Self::Agent { role, .. } | Self::Job { role, .. } => role,
        }
    }

    pub(super) fn end(self) -> Duration {
        match self {
            Self::Agent { deadline, .. } => deadline,
            Self::Job { end, .. } => end,
        }
    }
}

/// A grant of the caller's; a judge or submit grant carries the work phase's deadline.
enum Held {
    Judge(JudgeGrantId, Duration),
    Lease(LeaseGrantId),
    Submit(Duration),
}

impl Core<'_> {
    /// Answers `caller`'s op, and acts on it: the reply comes first, then the actions the op
    /// calls for.
    pub(super) fn op(
        &mut self,
        op: OpId,
        caller: Caller,
        request: Request,
        actions: &mut Vec<Action>,
    ) {
        let mut then = Vec::new();
        let reply =
            (self.working(caller)).and_then(|working| self.take(working, request, &mut then));
        let reply = reply.unwrap_or_else(|message| OpReply::Refused { message });
        actions.push(Action::Reply { op, reply });
        actions.append(&mut then);
    }

    /// `caller`, if it works: the agent in the work phase, a call while its command runs. Else
    /// why the run takes no more ops of `caller`'s.
    pub(super) fn working(&self, caller: Caller) -> Result<Working, String> {
        let work_end = match (&self.phase, caller) {
            (Phase::Abandoned, _) => {
                return Err("the run has passed its hard end: make no more ops".to_owned());
            }
            (_, Caller::Job(job)) => {
                return match self.jobs.get(job) {
                    Some(Job {
                        call,
                        timeout,
                        state: State::Running { started },
                    }) => Ok(Working::Job {
                        job,
                        role: self.workflow.call_slot(*call).judge().role(),
                        end: started.saturating_add(*timeout),
                    }),
                    Some(Job {
                        state: State::Queued | State::Ending { .. } | State::Ended { .. },
                        ..
                    })
                    | None => Err("the call's command has ended: make no more ops".to_owned()),
                };
            }
            (Phase::Work { deadline, .. }, Caller::Agent) => {
                return Ok(Working::Agent {
                    role: self.workflow.agent().role,
                    deadline: *deadline,
                });
            }
            (Phase::Finalized { .. }, Caller::Agent) => &WorkEnd::Finalized,
            (
                Phase::Teardown { work_end, .. }
                | Phase::Final { work_end, .. }
                | Phase::Finished { work_end },
                Caller::Agent,
            ) => work_end,
        };
        match work_end {
            WorkEnd::Finalized => {
                Err("a final submission was accepted: make no more ops".to_owned())
            }
            WorkEnd::Deadline | WorkEnd::AgentExited(_) | WorkEnd::AgentFailed(_) => {
                Err("the work phase has ended: make no more ops".to_owned())
            }
        }
    }

    /// Takes `request`, an op of `working`'s caller, and returns its reply, putting in `then` the
    /// actions to follow it; or why it is refused.
    fn take(
        &mut self,
        working: Working,
        request: Request,
        then: &mut Vec<Action>,
    ) -> Result<OpReply, String> {
        let (caller, end) = (working.caller(), working.end());
        match request {
            Request::Ops => Ok(OpReply::Ops(self.ops(working))),
            Request::Status { job } => self.own_job(caller, job).map(OpReply::Status),
            Request::Judge { grant, payload } => match self.held(working, &grant)? {
                Held::Judge(id, deadline) => {
                    self.judge(id, payload, deadline, then).map(OpReply::Judge)
                }
                other => Err(other.misnamed(&grant, "a judge grant")),
            },
            Request::Submit {
                grant,
                finalize,
                payload,
            } => match self.held(working, &grant)? {
                Held::Submit(deadline) => self
                    .submit(finalize, payload, deadline, then)
                    .map(OpReply::Submit),
                other => Err(other.misnamed(&grant, "a submit grant")),
            },
            Request::LeaseOpen {
                grant,
                instance_seconds,
            } => match self.held(working, &grant)? {
                Held::Lease(id) => {
                    let opened = self.lease_open((caller, id), end, instance_seconds, then);
                    opened.map(OpReply::Lease)
                }
                other => Err(other.misnamed(&grant, "a lease grant")),
            },
            Request::LeaseRun {
                lease,
                argv,
                timeout_seconds,
            } => {
                let ran = self.lease_run((caller, lease), end, argv, timeout_seconds, then);
                ran.map(OpReply::Lease)
            }
            Request::LeasePut {
                lease,
                tree,
                to,
                replace,
            } => {
                let direction = Direction::Put { tree, to, replace };
                (self.transfer((caller, lease), direction, then)).map(OpReply::Lease)
            }
            Request::LeaseGet { lease, from, to } => {
                let direction = Direction::Get { from, to };
                (self.transfer((caller, lease), direction, then)).map(OpReply::Lease)
            }
            Request::LeaseStatus { lease } => {
                let id = self.own_lease(caller, lease)?;
                Ok(OpReply::LeaseStatus(self.lease_status(id)))
            }
            Request::LeaseClose { lease } => {
                let id = self.own_lease(caller, lease)?;
                if self.leases[id].close(self.now, leases::End::Closed) {
                    then.push(Action::CloseLease { lease: id });
                }
                Ok(OpReply::Lease(self.lease_reply(id)))
            }
        }
    }

    /// `caller`'s lease `number`: one it opened.
    fn own_lease(&self, caller: Caller, number: u64) -> Result<LeaseId, String> {
        let lease = self.leases.find(number);
        (lease.filter(|lease| self.leases[*lease].holder == caller)).ok_or_else(|| {
            format!("you have no lease {number}: name one that `lease open` returned")
        })
    }

    /// The status of `caller`'s job `number`: one it made under a judge grant.
    fn own_job(&self, caller: Caller, number: u64) -> Result<StatusReply, String> {
        let job = JobId(number);
        let found = self
            .jobs
            .get(job)
            .and_then(|found| match (caller, found.call) {
                (Caller::Agent, JudgeCall::Grant(grant)) => Some((found, grant)),
                (Caller::Agent, JudgeCall::Score) | (Caller::Job(_), _) => None,
            });
        let Some((found, grant)) = found else {
            return Err(format!(
                "you have no job {number}: ask about a job that one of your calls returned"
            ));
        };
        Ok(self.status(job, found, grant))
    }

    /// The time from now until `end`, as a reply gives the time left
    /// ([`OpsReply::remaining_seconds`]): in whole seconds rounded down, and 0 once it has passed.
    fn seconds_left(&self, end: Duration) -> u64 {
        end.saturating_sub(self.now).as_secs()
    }

    fn held(&self, working: Working, name: &Name) -> Result<Held, String> {
        let role = working.role();
        // Parsing gives judge and submit grants to the agent alone (`Parser::roles`).
        let deadline = match working {
            Working::Agent { deadline, .. } => Some(deadline),
            Working::Job { .. } => None,
        };
        let judge = deadline.and_then(|deadline| {
            (self.workflow.judge_grants())
                .find(|(_, grant)| grant.name == *name)
                .map(|(id, _)| Held::Judge(id, deadline))
        });
        let lease = (self.workflow.lease_grants())
            .find(|(_, grant)| grant.holder == role && grant.name == *name)
            .map(|(id, _)| Held::Lease(id));
        let submit = deadline.filter(|_| self.workflow.submit_grant().name == *name);
        let held = judge.or(lease).or(submit.map(Held::Submit));
        held.ok_or_else(|| format!("you hold no grant named `{name}`: call one that `ops` lists"))
    }

    /// Makes a call of grant `id` on `payload`, before the work phase's `deadline`, a job, which
    /// `then` starts, and returns the reply that names it.
    fn judge(
        &mut self,
        id: JudgeGrantId,
        payload: PayloadRef,
        deadline: Duration,
        then: &mut Vec<Action>,
    ) -> Result<JudgeReply, String> {
        let grant = self.workflow.judge_grant(id);
        let name = &grant.name;
        fits(name, &payload, self.workflow.slot(grant.slot).max_bytes())?;
        let usage = self.jobs.usage(id);
        if let Some(calls) = grant.calls
            && usage.calls >= calls.get()
        {
            return Err(format!(
                "`{name}` has no calls left of the {calls} it allows: make no more calls to it"
            ));
        }
        // A call has ended only once every lease it held has.
        let holding = self
            .jobs
            .of(id)
            .any(|job| self.leases.holds(Caller::Job(job)));
        if usage.busy || holding {
            return Err(format!(
                "`{name}` runs one call at a time, and already has one queued or running: call \
                 again once it has finished; this call is not charged"
            ));
        }
        if let Some(total) = grant.cumulative()
            && usage.used >= total
        {
            return Err(format!(
                "`{name}` has no judge time left of the {} s it allows: make no more calls to it",
                total.as_secs()
            ));
        }
        // The work phase's end stops every call (`end_work`), so a call is given at most the time
        // the work phase has left when it is accepted; after a wait in the queue, its timeout can
        // reach past the deadline, where that stop ends it all the same. Each term is positive:
        // the work phase ends at the deadline, and every limit is left above zero.
        let work_left = deadline.saturating_sub(self.now);
        let judge_left =
            (grant.cumulative()).map_or(Duration::MAX, |total| total.saturating_sub(usage.used));
        let timeout = grant.call_timeout().min(judge_left).min(work_left);
        let job = self.jobs.add(JudgeCall::Grant(id), timeout);
        then.push(Action::StartJudge {
            job,
            call: JudgeCall::Grant(id),
            payload,
            timeout,
        });
        let remaining = self.remaining(id);
        Ok(JudgeReply {
            job: job.0,
            remaining,
        })
    }

    /// Opens a lease of grant `id` for `caller`, whose work ends at `end`, to last
    /// `instance_seconds` once ready, if given, or else the grant's own time, which `then` starts,
    /// and returns the reply that names it.
    fn lease_open(
        &mut self,
        (caller, id): (Caller, LeaseGrantId),
        end: Duration,
        instance_seconds: Option<NonZeroU64>,
        then: &mut Vec<Action>,
    ) -> Result<LeaseReply, String> {
        let grant = self.workflow.lease_grant(id);
        let (name, usage) = (&grant.name, self.leases.usage(id, caller, self.now));
        let most = grant.instance_seconds;
        if let Some(seconds) = instance_seconds
            && seconds > most
        {
            return Err(format!(
                "the time of {seconds} s is longer than `{name}` lets a lease last, {most} s: \
                 open it again as `sandboxctl lease open {name} --seconds {most}`"
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
                 another once `lease status` says one has ended"
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

    /// `caller`'s lease `number`, and since when its runner has been ready, if it may take its
    /// next command or transfer now, which the caller asked to `act`, as a refusal says it.
    fn idle_lease(
        &self,
        (caller, number): (Caller, u64),
        act: &str,
    ) -> Result<(LeaseId, Duration), String> {
        let id = self.own_lease(caller, number)?;
        let lease = &self.leases[id];
        let since = match lease.state {
            leases::State::Starting => {
                return Err(format!(
                    "lease {number} is still starting: {act} once `lease status` says it is ready"
                ));
            }
            leases::State::Ready { since } => since,
            leases::State::Closing { .. } | leases::State::Ended { .. } => {
                return Err(format!("lease {number} has ended: open another"));
            }
        };
        if let Some(running) = lease.running() {
            return Err(format!(
                "lease {number} runs {}, one at a time: {act} once `lease status` says it has \
                 ended",
                running.name()
            ));
        }
        let grant = self.workflow.lease_grant(lease.grant);
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

    /// Copies a tree as `direction` says, between the caller's sandbox and the runner of its
    /// lease `number`, as the lease's next transfer, which `then` starts, and returns the reply.
    fn transfer(
        &mut self,
        (caller, number): (Caller, u64),
        direction: Direction,
        then: &mut Vec<Action>,
    ) -> Result<LeaseReply, String> {
        let act = match direction {
            Direction::Put { .. } => "put a tree",
            Direction::Get { .. } => "get a tree",
        };
        let (id, _) = self.idle_lease((caller, number), act)?;
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
            max_bytes: self
                .workflow
                .lease_grant(self.leases[id].grant)
                .transfer_bytes,
        });
        Ok(self.lease_reply(id))
    }

    /// Runs `argv` in `caller`'s lease `number`, for at most `timeout_seconds`, if given, as its
    /// next command, which `then` starts, and returns the reply. `caller`'s work ends at
    /// `work_end`.
    fn lease_run(
        &mut self,
        (caller, number): (Caller, u64),
        work_end: Duration,
        argv: Argv,
        timeout_seconds: Option<NonZeroU64>,
        then: &mut Vec<Action>,
    ) -> Result<LeaseReply, String> {
        let (id, since) = self.idle_lease((caller, number), "run a command")?;
        let lease = &self.leases[id];
        let timeout = timeout_seconds.map(|seconds| Duration::from_secs(seconds.get()));
        // The lease ends by its own time, and by its holder's work's end.
        let end = self.ready_end(lease, since).min(work_end);
        let left = self.seconds_left(end);
        if let Some(timeout) = timeout_seconds
            && timeout.get() > left
        {
            let rerun = rerun(number, NonZeroU64::new(left), &argv);
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

    /// Stores a submission of `payload` as the latest, or locks it as the selection if
    /// `finalize`, settling when the work phase, whose deadline is `deadline`, ends ([`WorkEnd`]),
    /// and returns the reply, with the time left until then; `then` ends the work phase if the
    /// grace after a finalize is 0.
    fn submit(
        &mut self,
        finalize: bool,
        payload: PayloadRef,
        deadline: Duration,
        then: &mut Vec<Action>,
    ) -> Result<SubmitReply, String> {
        fits(
            &self.workflow.submit_grant().name,
            &payload,
            self.submission_max_bytes(),
        )?;
        // Ops are taken only while the agent works (`working`): the phase goes on with the
        // latest submission, or a finalize locks it.
        if !finalize {
            self.phase = Phase::Work {
                latest: Some(payload),
                deadline,
            };
            return Ok(SubmitReply {
                state: SubmitState::Stored,
                remaining_seconds: self.seconds_left(deadline),
            });
        }
        let grace = self.workflow.agent().finalize_grace;
        let grace_end = self.now.saturating_add(grace);
        let (end, work_end) = if grace_end < deadline {
            (grace_end, WorkEnd::Finalized)
        } else {
            (deadline, WorkEnd::Deadline)
        };
        self.phase = Phase::Finalized {
            payload,
            end,
            work_end,
        };
        if grace.is_zero() {
            self.end_work(WorkEnd::Finalized, then);
        }
        Ok(SubmitReply {
            state: SubmitState::Accepted,
            remaining_seconds: self.seconds_left(end),
        })
    }

    /// The most bytes a submission may hold: the score slot's, so that whatever is selected
    /// fits it.
    fn submission_max_bytes(&self) -> NonZeroU64 {
        self.workflow.slot(self.workflow.score().slot).max_bytes()
    }

    fn ops(&self, working: Working) -> OpsReply {
        let (caller, role, end) = (working.caller(), working.role(), working.end());
        let agent = matches!(working, Working::Agent { .. });
        let judges = (self.workflow.judge_grants())
            .filter(|_| agent)
            .map(|(_, grant)| Grant::Judge(self.judge_grant(grant)));
        let leases = (self.workflow.lease_grants()).filter(|(_, grant)| grant.holder == role);
        let leases = leases.map(|(id, grant)| {
            let LeaseGrant {
                name,
                holder: _,
                runner: _,
                instances,
                instance_seconds,
                cumulative_seconds,
                commands,
                transfer_bytes,
            } = grant;
            Grant::Lease(op::LeaseGrant {
                name: name.clone(),
                instances: *instances,
                instance_seconds: *instance_seconds,
                cumulative_seconds: *cumulative_seconds,
                commands: *commands,
                transfer_bytes: *transfer_bytes,
                remaining: self.grant_remaining((id, caller), grant.instance()),
            })
        });
        let submit = agent.then(|| {
            Grant::Submit(op::SubmitGrant {
                name: self.workflow.submit_grant().name.clone(),
                max_bytes: self.submission_max_bytes(),
            })
        });
        OpsReply {
            remaining_seconds: self.seconds_left(end),
            resources: (self.workflow.sandbox(role).needs).resources(),
            grants: judges.chain(leases).chain(submit).collect(),
        }
    }

    fn judge_grant(&self, grant: &JudgeGrant) -> op::JudgeGrant {
        let JudgeGrant {
            name,
            slot,
            calls,
            cumulative_seconds,
            call_timeout_seconds,
        } = grant;
        op::JudgeGrant {
            name: name.clone(),
            max_bytes: self.workflow.slot(*slot).max_bytes(),
            calls: *calls,
            cumulative_seconds: *cumulative_seconds,
            call_timeout_seconds: *call_timeout_seconds,
        }
    }

    fn status(&self, id: JobId, job: &Job, grant: JudgeGrantId) -> StatusReply {
        let state = match &job.state {
            State::Queued => JobState::Queued,
            State::Running { .. } | State::Ending { .. } => JobState::Running,
            // A call has ended only once every lease it held has.
            State::Ended { .. } if self.leases.holds(Caller::Job(id)) => JobState::Running,
            State::Ended {
                outcome: Ok(result),
                ..
            } => result.job_state(),
            State::Ended {
                outcome: Err(failure),
                ..
            } => JobState::Errored {
                failure: failure.reply(),
            },
        };
        StatusReply {
            job: id.0,
            state,
            remaining: self.remaining(grant),
        }
    }

    fn lease_reply(&self, id: LeaseId) -> LeaseReply {
        LeaseReply {
            lease: id.0,
            remaining: self.lease_remaining(id),
        }
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
            leases::State::Starting => lease.life,
            leases::State::Ready { since } => {
                (lease.life).saturating_sub(self.now.saturating_sub(since))
            }
            leases::State::Closing { .. } | leases::State::Ended { .. } => Duration::ZERO,
        };
        self.grant_remaining((lease.grant, lease.holder), own)
    }

    /// What grant `id` has left of its limits for `holder`, with `own` as a lease's own time
    /// left.
    fn grant_remaining(
        &self,
        (id, holder): (LeaseGrantId, Caller),
        own: Duration,
    ) -> LeaseRemaining {
        let grant = self.workflow.lease_grant(id);
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

    fn remaining(&self, id: JudgeGrantId) -> Remaining {
        let (grant, usage) = (self.workflow.judge_grant(id), self.jobs.usage(id));
        Remaining {
            calls: (grant.calls).map(|calls| calls.get().saturating_sub(usage.calls)),
            seconds: (grant.cumulative()).map(|total| total.saturating_sub(usage.used).as_secs()),
        }
    }
}

impl Held {
    /// Why naming this grant, `name`, is refused for an op that takes `kind` of grant.
    fn misnamed(&self, name: &Name, kind: &str) -> String {
        let (is, instead) = match self {
            Self::Judge(..) => ("a judge grant", "judge with it"),
            Self::Lease(_) => ("a lease grant", "open leases of it"),
            Self::Submit(_) => ("a submit grant", "submit to it"),
        };
        format!("`{name}` is {is}, not {kind}: {instead} instead")
    }
}

/// The start of `text`, of at most [`REASON_BYTES`], as a lease's status gives a runner's failure.
fn cut(text: &str) -> String {
    let mut end = REASON_BYTES.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// The command that runs `argv` in lease `lease`, with `timeout`, if given, as a shell takes it.
fn rerun(lease: u64, timeout: Option<NonZeroU64>, argv: &Argv) -> String {
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
        "sandboxctl lease run {lease}{timeout} -- {}",
        words.join(" ")
    )
}

/// Refuses a payload that is empty, or over `max_bytes`.
fn fits(name: &Name, payload: &PayloadRef, max_bytes: NonZeroU64) -> Result<(), String> {
    if payload.size == 0 {
        Err(format!(
            "the payload is empty: send `{name}` at least one byte"
        ))
    } else if payload.size <= max_bytes.get() {
        Ok(())
    } else {
        Err(format!(
            "the payload is {} bytes, over `{name}`'s limit of {max_bytes}: send a smaller one",
            payload.size
        ))
    }
}
