//! Each member owns its selection and finalize grace. The work phase lasts until every
//! member's sandbox, jobs and leases have ended. The final body then sees their selections in
//! declaration order.

use alloc::vec::Vec;
use core::time::Duration;

use schema::workflow::{AgentId, JobOf};

use crate::{
    Action, CallResult, Caller, Core, Failure, FinalInput, MemberOutcome, Outcome, PayloadRef,
    Phase, Submission, Verdict, WorkEnd, jobs,
};

#[derive(Debug)]
pub(super) enum Member {
    Working(Option<PayloadRef>),
    Grace {
        selected: PayloadRef,
        end: Duration,
    },
    Ended {
        outcome: MemberOutcome,
        torn_down: bool,
    },
}

impl Member {
    pub(super) fn locked(&self) -> bool {
        match self {
            Self::Working(_) => false,
            Self::Grace { .. } => true,
            Self::Ended { outcome, .. } => outcome.submission.as_ref().is_some_and(|s| s.finalized),
        }
    }
}

#[derive(Debug)]
pub(super) enum Judging {
    Single {
        work_end: WorkEnd,
        submission: Submission,
    },
    Team(Vec<MemberOutcome>),
}

impl Judging {
    fn outcome(&self, result: Result<CallResult, Failure>) -> Outcome {
        match self {
            Self::Single {
                work_end,
                submission,
            } => {
                let submission = submission.clone();
                let verdict = match result {
                    Ok(result) => Verdict::Returned { submission, result },
                    Err(failure) => Verdict::NoResult {
                        submission,
                        failure,
                    },
                };
                Outcome::Single {
                    work_end: work_end.clone(),
                    verdict,
                }
            }
            Self::Team(members) => Outcome::Team {
                members: members.clone(),
                result,
            },
        }
    }
}

impl Core<'_> {
    pub(super) fn work_due(&self, agent: AgentId) -> Option<(Duration, WorkEnd)> {
        let Phase::Work { deadline } = self.phase else {
            return None;
        };
        match self.members.get(&agent)? {
            Member::Grace { end, .. } if *end < deadline => Some((*end, WorkEnd::Finalized)),
            Member::Working(_) | Member::Grace { .. } => Some((deadline, WorkEnd::Deadline)),
            Member::Ended { .. } => None,
        }
    }

    pub(super) fn end_due_work(&mut self, actions: &mut Vec<Action>) {
        for agent in self.workflow.agents() {
            if let Some((end, reason)) = self.work_due(agent.id)
                && self.now >= end
            {
                self.end_work(agent.id, reason, actions);
            }
        }
    }

    pub(super) fn end_work(
        &mut self,
        agent: AgentId,
        work_end: WorkEnd,
        actions: &mut Vec<Action>,
    ) {
        if !matches!(self.phase, Phase::Work { .. }) {
            return;
        }
        let Some(member) = self.members.get_mut(&agent) else {
            return;
        };
        let submission = match member {
            Member::Working(latest) => latest.take().map(|payload| Submission {
                payload,
                finalized: false,
            }),
            Member::Grace { selected, .. } => Some(Submission {
                payload: selected.clone(),
                finalized: true,
            }),
            Member::Ended { .. } => return,
        };
        *member = Member::Ended {
            outcome: MemberOutcome {
                agent,
                route_failure: None,
                work_end,
                submission,
            },
            // A scalar teardown completes before the next action. A team teardown reports back.
            torn_down: !self.workflow.final_call().selections() && self.shared.is_empty(),
        };

        let owns = |call| match call {
            JobOf::Function(function) => self.workflow.job_function(function).holder == agent,
            JobOf::Final => false,
        };
        let stopped = self.jobs.stop_unfinished(self.now, owns);
        actions.extend(stopped.into_iter().map(|job| Action::StopCall { job }));
        let closed = self
            .leases
            .close_where(self.now, |lease| match lease.holder {
                Caller::Agent(holder) => holder == agent,
                Caller::Job(job) => self.jobs.get(job).is_some_and(|job| owns(job.call)),
            });
        actions.extend(closed.into_iter().map(|lease| Action::CloseLease { lease }));
        actions.push(Action::TearDownAgent { agent });
    }

    pub(super) fn advance(&mut self, actions: &mut Vec<Action>) {
        match &self.phase {
            Phase::Work { .. } => {
                if self.members.values().all(|member| {
                    matches!(
                        member,
                        Member::Ended {
                            torn_down: true,
                            ..
                        }
                    )
                }) && self.jobs.all_ended()
                    && self.leases.all_ended()
                {
                    if self.shared.is_empty() {
                        self.enter_final(actions);
                    } else {
                        for (target, service) in &mut self.shared {
                            if *service != crate::Service::Stopping {
                                *service = crate::Service::Stopping;
                                actions.push(Action::TearDownShared { target: *target });
                            }
                        }
                    }
                }
            }
            Phase::Final { judging, job } => {
                if let Some(jobs::Job {
                    state: jobs::State::Ended { outcome, .. },
                    ..
                }) = self.jobs.get(*job)
                    && self.leases.all_ended()
                {
                    self.finish(judging.outcome(outcome.clone()), actions);
                }
            }
            Phase::Finished | Phase::Abandoned => {}
        }
    }

    fn enter_final(&mut self, actions: &mut Vec<Action>) {
        // Agent IDs compare by declaration order, so the table and manifest have this order too.
        let members: Vec<_> = self
            .members
            .values()
            .filter_map(|member| match member {
                Member::Ended { outcome, .. } => Some(outcome.clone()),
                Member::Working(_) | Member::Grace { .. } => None,
            })
            .collect();
        let final_call = self.workflow.final_call();
        let (input, judging) = if let [member] = members.as_slice()
            && !final_call.selections()
        {
            let work_end = member.work_end.clone();
            let Some(submission) = &member.submission else {
                return self.finish(
                    Outcome::Single {
                        work_end,
                        verdict: Verdict::NoSubmission,
                    },
                    actions,
                );
            };
            (
                FinalInput::Payload(submission.payload.clone()),
                Judging::Single {
                    work_end,
                    submission: submission.clone(),
                },
            )
        } else {
            let selections = members
                .iter()
                .map(|member| {
                    (
                        member.agent,
                        member.submission.as_ref().map(|s| s.payload.clone()),
                    )
                })
                .collect();
            (FinalInput::Selections(selections), Judging::Team(members))
        };
        let timeout = final_call.timeout;
        let job = self.jobs.add(JobOf::Final, timeout);
        actions.push(Action::StartFinalCall {
            job,
            input,
            timeout,
        });
        self.phase = Phase::Final { judging, job };
    }
}
