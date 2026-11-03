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
//! - A judge grant has calls left, fewer calls running than its concurrency, and judge time
//!   left. A refused call is not charged.
//! - A status names one of the agent's jobs.
//!
//! A judge call that passes becomes a job, which the reply names, and runs in the background
//! with the timeout that
//! [`call_timeout_seconds`](pumpkin_schema::ir::JudgeGrant::call_timeout_seconds) describes.
//!
//! Each op the core takes gets the reply for its kind, as [`op`] describes it, the time left
//! included ([`OpsReply::remaining_seconds`], [`SubmitReply::remaining_seconds`]). `ops` lists
//! the agent's judge grants first, then its submit grant.

use std::num::NonZeroU64;
use std::time::Duration;

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
enum Authorized<'c> {
    Ops,
    /// A job of the agent's, made under one of its grants.
    Status(JobId, JudgeGrantId, &'c Job),
    Judge {
        grant: JudgeGrantId,
        payload: PayloadRef,
        timeout_ms: u64,
    },
    Submit(Submission),
}

/// A grant of the agent's.
enum Held {
    Judge(JudgeGrantId),
    Submit,
}

impl Core<'_> {
    /// Answers an op, and acts on it.
    pub(super) fn op(&mut self, op: OpId, request: Request, actions: &mut Vec<Action>) {
        let authorized = match self.authorize(request) {
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
            Authorized::Status(job, grant, found) => {
                let reply = OpReply::Status(self.status(job, grant, found));
                actions.push(Action::Reply { op, reply });
            }
            Authorized::Judge {
                grant,
                payload,
                timeout_ms,
            } => {
                let job = self.jobs.add(JudgeCall::Grant(grant));
                let remaining = self.remaining(grant);
                let reply = OpReply::Judge(JudgeReply {
                    job: job.0,
                    remaining,
                });
                actions.push(Action::Reply { op, reply });
                let timeout = Duration::from_millis(timeout_ms);
                actions.push(Action::StartJudge {
                    job,
                    call: JudgeCall::Grant(grant),
                    payload,
                    timeout,
                });
            }
            Authorized::Submit(submission) => {
                let (state, end_ms) = if submission.finalized {
                    let grace_end_ms = self.now_ms.saturating_add(self.finalize_grace_ms());
                    (SubmitState::Accepted, grace_end_ms.min(self.deadline_ms()))
                } else {
                    (SubmitState::Stored, self.deadline_ms())
                };
                let remaining_seconds = end_ms.saturating_sub(self.now_ms) / 1000;
                let reply = OpReply::Submit(SubmitReply {
                    state,
                    remaining_seconds,
                });
                actions.push(Action::Reply { op, reply });
                if submission.finalized {
                    self.finalize(submission, actions);
                } else {
                    #[expect(
                        clippy::unreachable,
                        reason = "only the work phase authorizes a submission"
                    )]
                    let Phase::Work { submission: latest } = &mut self.phase else {
                        unreachable!("a submission was authorized in {:?}", self.phase)
                    };
                    *latest = Some(submission);
                }
            }
        }
    }

    fn authorize(&self, request: Request) -> Result<Authorized<'_>, String> {
        let work_end = match &self.phase {
            Phase::Work { .. } => None,
            Phase::Finalized { .. } => Some(&WorkEnd::Finalized),
            Phase::Teardown { work_end, .. }
            | Phase::Final { work_end, .. }
            | Phase::Finished { work_end } => Some(work_end),
        };
        match work_end {
            None => {}
            Some(WorkEnd::Finalized) => {
                return Err("a final submission was accepted: no more ops are taken".to_owned());
            }
            Some(WorkEnd::Deadline | WorkEnd::AgentExited(_) | WorkEnd::AgentFailed(_)) => {
                return Err("the work phase has ended: no more ops are taken".to_owned());
            }
        }
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

    /// The agent's job `number`.
    fn own_job(&self, number: u64) -> Result<Authorized<'_>, String> {
        let job = JobId(number);
        let Some(found) = self.jobs.get(job) else {
            return Err(format!(
                "you have no job {number}: ask about a job that one of your calls returned"
            ));
        };
        match found.call {
            JudgeCall::Grant(grant) => Ok(Authorized::Status(job, grant, found)),
            #[expect(
                clippy::unreachable,
                reason = "the score's job starts once the work phase has ended, and only the work \
                          phase takes ops"
            )]
            JudgeCall::Score => unreachable!("job {number}, the score's, was asked about"),
        }
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

    fn authorize_judge(
        &self,
        id: JudgeGrantId,
        payload: PayloadRef,
    ) -> Result<Authorized<'_>, String> {
        let grant = self.workflow.judge_grant(id);
        let name = &grant.name;
        fits(name, &payload, self.workflow.slot(grant.slot).max_bytes)?;
        let usage = self.jobs.usage(id);
        if let Some(calls) = grant.calls
            && usage.calls >= calls.get()
        {
            return Err(format!("`{name}` has no calls left: it allows {calls}"));
        }
        if usage.running >= u64::from(grant.concurrency.get()) {
            return Err(format!(
                "`{name}` already has {} call(s) running, as many as it allows at once: call \
                 again once one has finished; this call is not charged",
                usage.running
            ));
        }
        if let Some(total) = grant.cumulative_seconds
            && usage.used_ms >= total.get().saturating_mul(1000)
        {
            return Err(format!(
                "`{name}` has no judge time left: its calls have used the {total} s it allows"
            ));
        }
        // Positive: the work phase ends at the deadline, and every limit is left above zero.
        let run_left_ms = self.deadline_ms().saturating_sub(self.now_ms);
        let timeout_ms = self.call_limit_ms(id).min(run_left_ms);
        Ok(Authorized::Judge {
            grant: id,
            payload,
            timeout_ms,
        })
    }

    fn authorize_submit(
        &self,
        finalize: bool,
        payload: PayloadRef,
    ) -> Result<Authorized<'_>, String> {
        let name = &self.workflow.submit_grant().name;
        fits(name, &payload, self.submission_max_bytes())?;
        Ok(Authorized::Submit(Submission {
            payload,
            finalized: finalize,
        }))
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
            remaining_seconds: self.deadline_ms().saturating_sub(self.now_ms) / 1000,
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
            concurrency,
            call_timeout_seconds,
        } = grant;
        op::JudgeGrant {
            name: name.clone(),
            max_bytes: self.workflow.slot(*slot).max_bytes,
            calls: *calls,
            cumulative_seconds: *cumulative_seconds,
            concurrency: *concurrency,
            call_timeout_seconds: *call_timeout_seconds,
        }
    }

    /// The reply to `status`, of a job made under `grant`.
    fn status(&self, id: JobId, grant: JudgeGrantId, job: &Job) -> StatusReply {
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
            remaining: self.remaining(grant),
        }
    }

    /// What `grant` has left of its quotas.
    fn remaining(&self, grant: JudgeGrantId) -> Remaining {
        let calls = self.workflow.judge_grant(grant).calls;
        let used = self.jobs.usage(grant).calls;
        Remaining {
            calls: calls.map(|calls| calls.get().saturating_sub(used)),
            seconds: self.cumulative_left_ms(grant).map(|ms| ms / 1000),
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
