//! Authorizing ops, and the replies to them.
//!
//! The core authorizes every op from the raw request ([`Request`]) and refuses, with a reason,
//! one that fails any of these:
//!
//! - Ops are taken only during the work phase, and refused once a finalize is accepted
//!   ([`finalize_grace_seconds`](pumpkin_schema::ir::SoloThenJudge::finalize_grace_seconds)).
//! - A judge call names one of the agent's judge grants, and a submission its submit grant.
//! - Its payload isn't empty, and fits its slot's
//!   [`max_bytes`](pumpkin_schema::ir::Slot::max_bytes), the score's for a submission.
//! - A judge grant has calls left, none of its calls queued or running, and judge time left.
//!   A refused call is not charged.
//! - A status names one of the agent's jobs.
//!
//! A judge call that passes becomes a job, which the reply names, and runs in the background
//! with the timeout that
//! [`call_timeout_seconds`](pumpkin_schema::ir::JudgeGrant::call_timeout_seconds) describes.
//!
//! Each op the core takes gets the reply for its kind, as [`op`] describes it, the time left
//! included ([`OpsReply::remaining_seconds`], [`SubmitReply::remaining_seconds`]). `ops` lists
//! the agent's judge grants first, then its submit grant.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroU64;
use core::time::Duration;

use pumpkin_schema::ir::Name;
use pumpkin_schema::sandbox::op::{
    self, Failed, Grant, JobState, JudgeReply, OpReply, OpsReply, Remaining, StatusReply,
    SubmitReply, SubmitState,
};
use pumpkin_schema::workflow::{JudgeCall, JudgeGrant, JudgeGrantId};

use super::jobs::{Job, State};
use super::{Action, Core, JobId, OpId, PayloadRef, Phase, Request, Submission, WorkEnd};

/// An op the core has authorized: its request, parsed against the workflow and the run's
/// state.
enum Authorized {
    Ops,
    /// The reply about a job of the agent's.
    Status(StatusReply),
    Judge {
        grant: JudgeGrantId,
        payload: PayloadRef,
        timeout: Duration,
    },
    /// A submission, and the reply that accepts or stores it.
    Submit(Submission, SubmitReply),
}

/// A grant of the agent's.
enum Held {
    Judge(JudgeGrantId),
    Submit,
}

impl Core<'_> {
    /// Answers an op, and acts on it.
    pub(super) fn op(&mut self, op: OpId, request: Request, actions: &mut Vec<Action>) {
        // The request is authorized before the phase is matched, so that the match can hand back
        // the work phase's latest submission; a phase that takes no ops still refuses first.
        let authorized = self.authorize(request);
        let authorized = (self.taking_ops()).and_then(|latest| Ok((authorized?, latest)));
        let (authorized, latest) = match authorized {
            Ok(authorized) => authorized,
            Err(message) => {
                let reply = OpReply::Refused { message };
                actions.push(Action::Reply { op, reply });
                return;
            }
        };
        match authorized {
            Authorized::Ops => {
                let reply = OpReply::Ops(self.ops());
                actions.push(Action::Reply { op, reply });
            }
            Authorized::Status(reply) => {
                let reply = OpReply::Status(reply);
                actions.push(Action::Reply { op, reply });
            }
            Authorized::Judge {
                grant,
                payload,
                timeout,
            } => {
                let job = self.jobs.add(grant);
                let remaining = self.remaining(grant);
                let reply = OpReply::Judge(JudgeReply {
                    job: job.0,
                    remaining,
                });
                actions.push(Action::Reply { op, reply });
                actions.push(Action::StartJudge {
                    job,
                    call: JudgeCall::Grant(grant),
                    payload,
                    timeout,
                });
            }
            Authorized::Submit(submission, reply) => {
                let reply = OpReply::Submit(reply);
                actions.push(Action::Reply { op, reply });
                if submission.finalized {
                    self.finalize(submission, actions);
                } else {
                    *latest = Some(submission);
                }
            }
        }
    }

    /// The work phase's latest submission, or why the run takes no more ops.
    fn taking_ops(&mut self) -> Result<&mut Option<Submission>, String> {
        let work_end = match &mut self.phase {
            Phase::Work { submission } => return Ok(submission),
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

    fn authorize(&self, request: Request) -> Result<Authorized, String> {
        match request {
            Request::Ops => Ok(Authorized::Ops),
            Request::Status { job } => self.own_job(job),
            Request::Judge { grant, payload } => match self.held(&grant)? {
                Held::Judge(id) => self.authorize_judge(id, payload),
                Held::Submit => Err(format!(
                    "`{grant}` is a submit grant, not a judge grant: submit to it instead"
                )),
            },
            Request::Submit {
                grant,
                finalize,
                payload,
            } => match self.held(&grant)? {
                Held::Submit => self.authorize_submit(finalize, payload),
                Held::Judge(_) => Err(format!(
                    "`{grant}` is a judge grant, not a submit grant: judge with it instead"
                )),
            },
        }
    }

    /// The reply about the agent's job `number`.
    fn own_job(&self, number: u64) -> Result<Authorized, String> {
        let job = JobId(number);
        let Some(found) = self.jobs.get(job) else {
            return Err(format!(
                "you have no job {number}: ask about a job that one of your calls returned"
            ));
        };
        Ok(Authorized::Status(self.status(job, found)))
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

    fn authorize_judge(&self, id: JudgeGrantId, payload: PayloadRef) -> Result<Authorized, String> {
        let grant = self.workflow.judge_grant(id);
        let name = &grant.name;
        fits(name, &payload, self.workflow.slot(grant.slot).max_bytes)?;
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
        // Positive: the work phase ends at the deadline, and every limit is left above zero.
        let run_left = self.pattern.deadline.saturating_sub(self.now);
        let timeout = self.call_limit(id).min(run_left);
        Ok(Authorized::Judge {
            grant: id,
            payload,
            timeout,
        })
    }

    fn authorize_submit(&self, finalize: bool, payload: PayloadRef) -> Result<Authorized, String> {
        let name = &self.workflow.submit_grant().name;
        fits(name, &payload, self.submission_max_bytes())?;
        let (state, end) = if finalize {
            let grace_end = self.now.saturating_add(self.pattern.finalize_grace);
            (SubmitState::Accepted, grace_end.min(self.pattern.deadline))
        } else {
            (SubmitState::Stored, self.pattern.deadline)
        };
        let remaining_seconds = end.saturating_sub(self.now).as_secs();
        let submission = Submission {
            payload,
            finalized: finalize,
        };
        let reply = SubmitReply {
            state,
            remaining_seconds,
        };
        Ok(Authorized::Submit(submission, reply))
    }

    /// The most bytes a submission may hold: the score slot's, so that whatever is selected
    /// fits it.
    fn submission_max_bytes(&self) -> NonZeroU64 {
        self.workflow.slot(self.pattern.score.slot).max_bytes
    }

    /// The reply to `ops`.
    fn ops(&self) -> OpsReply {
        let judges =
            (self.workflow.judge_grants()).map(|(_, grant)| Grant::Judge(self.judge_grant(grant)));
        let submit = Grant::Submit(op::SubmitGrant {
            name: self.workflow.submit_grant().name.clone(),
            max_bytes: self.submission_max_bytes(),
        });
        OpsReply {
            remaining_seconds: self.pattern.deadline.saturating_sub(self.now).as_secs(),
            resources: self.workflow.agent().sandbox.resources,
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
            max_bytes: self.workflow.slot(*slot).max_bytes,
            calls: *calls,
            cumulative_seconds: *cumulative_seconds,
            call_timeout_seconds: *call_timeout_seconds,
        }
    }

    /// The reply to `status`, of the agent's job `id`.
    fn status(&self, id: JobId, job: &Job) -> StatusReply {
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
            } => JobState::Failed(Failed::NoResult {
                failure: failure.reply(),
            }),
        };
        StatusReply {
            job: id.0,
            state,
            remaining: self.remaining(job.grant),
        }
    }

    /// What `grant` has left of its quotas.
    fn remaining(&self, grant: JudgeGrantId) -> Remaining {
        let calls = self.workflow.judge_grant(grant).calls;
        let used = self.jobs.usage(grant).calls;
        Remaining {
            calls: calls.map(|calls| calls.get().saturating_sub(used)),
            seconds: self.cumulative_left(grant).as_ref().map(Duration::as_secs),
        }
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
