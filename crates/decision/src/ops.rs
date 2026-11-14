//! Authorizing ops, and the replies to them.
//!
//! The core authorizes every op from the raw request ([`Request`]) and refuses, with a reason,
//! one that fails any of these:
//!
//! - Ops are taken only during the work phase, and refused once a finalize is accepted
//!   ([`finalize_grace_seconds`](schema::ir::AgentRuntime::finalize_grace_seconds)).
//! - A judge call names one of the agent's judge grants, and a submission its submit grant.
//! - Its payload isn't empty, and fits its slot's
//!   [`max_bytes`](schema::ir::Slot::max_bytes), the score's for a submission.
//! - A judge grant has calls left, none of its calls queued or running, and judge time left.
//!   A refused call is not charged.
//! - A status names one of the agent's jobs.
//!
//! A judge call that passes becomes a job, run in the background with the timeout that
//! [`call_timeout_seconds`](schema::ir::JudgeGrant::call_timeout_seconds) describes. Each reply
//! gives the time left until the agent's deadline.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroU64;
use core::time::Duration;

use schema::ir::Name;
use schema::sandbox::op::{
    self, Grant, JobState, JudgeReply, OpReply, OpsReply, Remaining, StatusReply, SubmitReply,
    SubmitState,
};
use schema::workflow::{JudgeCall, JudgeGrant, JudgeGrantId};

use super::jobs::{Job, State};
use super::{Action, Core, JobId, OpId, PayloadRef, Phase, Request, WorkEnd};

/// A grant of the agent's.
enum Held {
    Judge(JudgeGrantId),
    Submit,
}

impl Core<'_> {
    /// Answers an op, and acts on it: the reply comes first, then the actions the op calls for.
    pub(super) fn op(&mut self, op: OpId, request: Request, actions: &mut Vec<Action>) {
        let mut then = Vec::new();
        let reply =
            (self.works_until()).and_then(|deadline| self.take(deadline, request, &mut then));
        let reply = reply.unwrap_or_else(|message| OpReply::Refused { message });
        actions.push(Action::Reply { op, reply });
        actions.append(&mut then);
    }

    /// When the agent's work ends, unless it ends it first, as a time since the run started, if
    /// it works: at the deadline, in the work phase. Else why the run takes no more ops.
    fn works_until(&self) -> Result<Duration, String> {
        let work_end = match &self.phase {
            Phase::Work { deadline, .. } => return Ok(*deadline),
            Phase::Finalized { .. } => &WorkEnd::Finalized,
            Phase::Teardown { work_end, .. }
            | Phase::Final { work_end, .. }
            | Phase::Finished { work_end } => work_end,
            Phase::Abandoned => {
                return Err("the run has passed its hard end: make no more ops".to_owned());
            }
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

    /// Takes `request`, an op made while the agent works, until the work phase's `deadline`, and
    /// returns its reply, putting in `then` the actions to follow it; or why it is refused.
    fn take(
        &mut self,
        deadline: Duration,
        request: Request,
        then: &mut Vec<Action>,
    ) -> Result<OpReply, String> {
        match request {
            Request::Ops => Ok(OpReply::Ops(self.ops(deadline))),
            Request::Status { job } => self.own_job(job).map(OpReply::Status),
            Request::Judge { grant, payload } => match self.held(&grant)? {
                Held::Judge(id) => self.judge(id, payload, deadline, then).map(OpReply::Judge),
                other @ Held::Submit => Err(other.misnamed(&grant, "a judge grant")),
            },
            Request::Submit {
                grant,
                finalize,
                payload,
            } => match self.held(&grant)? {
                Held::Submit => self
                    .submit(finalize, payload, deadline, then)
                    .map(OpReply::Submit),
                other @ Held::Judge(_) => Err(other.misnamed(&grant, "a submit grant")),
            },
        }
    }

    /// The reply about the agent's job `number`.
    fn own_job(&self, number: u64) -> Result<StatusReply, String> {
        let job = JobId(number);
        let found = (self.jobs.get(job)).and_then(|found| match found.call {
            JudgeCall::Grant(grant) => Some((found, grant)),
            JudgeCall::Score => None,
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

    /// The agent's grant named `name`.
    fn held(&self, name: &Name) -> Result<Held, String> {
        let judge = (self.workflow.judge_grants())
            .find(|(_, grant)| grant.name == *name)
            .map(|(id, _)| Held::Judge(id));
        let submit = self.workflow.submit_grant().name == *name;
        let held = judge.or(submit.then_some(Held::Submit));
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
        if usage.busy {
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
        let job = self.jobs.add(JudgeCall::Grant(id));
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
        // Ops are taken only while the agent works (`works_until`): the phase goes on with the
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

    /// The reply to `ops`, while the agent works, until the work phase's `deadline`.
    fn ops(&self, deadline: Duration) -> OpsReply {
        let judges =
            (self.workflow.judge_grants()).map(|(_, grant)| Grant::Judge(self.judge_grant(grant)));
        let submit = Grant::Submit(op::SubmitGrant {
            name: self.workflow.submit_grant().name.clone(),
            max_bytes: self.submission_max_bytes(),
        });
        OpsReply {
            remaining_seconds: self.seconds_left(deadline),
            resources: self
                .workflow
                .sandbox(self.workflow.agent().role)
                .needs
                .resources(),
            grants: judges.chain([submit]).collect(),
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

    /// The reply to `status`, of the agent's job `id`, a call of grant `grant`.
    fn status(&self, id: JobId, job: &Job, grant: JudgeGrantId) -> StatusReply {
        let state = match &job.state {
            State::Queued => JobState::Queued,
            State::Running { .. } | State::Ending { .. } => JobState::Running,
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

    /// What grant `id` has left of its quotas.
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
            Self::Judge(_) => ("a judge grant", "judge with it"),
            Self::Submit => ("a submit grant", "submit to it"),
        };
        format!("`{name}` is {is}, not {kind}: {instead} instead")
    }
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
