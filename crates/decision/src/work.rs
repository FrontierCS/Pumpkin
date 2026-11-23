//! Each agent owns its submission and finalize grace. The final body receives every submission,
//! in declaration order, once all agents and jobs are torn down, all leases have ended and the
//! work targets' sandboxes are deleted. The run finishes once the final call is torn down too.

use alloc::vec::Vec;
use core::time::Duration;

use schema::workflow::{AgentId, JobOf};

use crate::{
    Action, AgentOutcome, Caller, Core, Outcome, PayloadRef, Phase, Submission, WorkEnd, jobs,
};

#[derive(Debug, Default)]
pub(super) struct AgentWork {
    pub(super) submission: Option<PayloadRef>,
    /// Set by `run.end_work()`, even if the agent submitted nothing.
    pub(super) grace_end: Option<Duration>,
    pub(super) ended: Option<Ended>,
}

#[derive(Debug)]
pub(super) struct Ended {
    pub(super) reason: WorkEnd,
    pub(super) torn_down: bool,
}

impl AgentWork {
    fn outcome(&self, agent: AgentId) -> AgentOutcome {
        let Some(end) = &self.ended else {
            unreachable!("the final call starts only once every agent has ended and torn down")
        };
        AgentOutcome {
            agent,
            work_end: end.reason.clone(),
            submission: self.submission.clone().map(|payload| Submission {
                payload,
                finalized: self.grace_end.is_some(),
            }),
            route_failure: None,
        }
    }
}

impl Core<'_> {
    pub(super) fn work_due(&self, agent: AgentId) -> Option<(Duration, WorkEnd)> {
        let Phase::Work { deadline } = self.phase else {
            return None;
        };
        let work = self.agents.get(&agent)?;
        if work.ended.is_some() {
            return None;
        }
        match work.grace_end {
            Some(end) if end < deadline => Some((end, WorkEnd::Finalized)),
            _ => Some((deadline, WorkEnd::Deadline)),
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

    pub(super) fn end_work(&mut self, agent: AgentId, reason: WorkEnd, actions: &mut Vec<Action>) {
        if !matches!(self.phase, Phase::Work { .. }) {
            return;
        }
        let Some(work) = self
            .agents
            .get_mut(&agent)
            .filter(|work| work.ended.is_none())
        else {
            return;
        };
        work.ended = Some(Ended {
            reason,
            torn_down: false,
        });

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
        match self.phase {
            Phase::Work { .. } => {
                if self
                    .agents
                    .values()
                    .all(|work| work.ended.as_ref().is_some_and(|end| end.torn_down))
                    && self.jobs.all_torn_down()
                    && self.leases.all_ended()
                {
                    if !self.shared.is_empty() {
                        for (target, service) in &mut self.shared {
                            if *service != crate::Availability::Stopping {
                                *service = crate::Availability::Stopping;
                                actions.push(Action::TearDownShared { target: *target });
                            }
                        }
                        return;
                    }
                    let selections = self
                        .agents
                        .iter()
                        .map(|(&agent, work)| (agent, work.submission.clone()))
                        .collect();
                    let timeout = self.workflow.final_call().timeout;
                    let job = self.jobs.add(JobOf::Final, timeout);
                    actions.push(Action::StartFinalCall {
                        job,
                        selections,
                        timeout,
                    });
                    self.phase = Phase::Final { job };
                }
            }
            Phase::Final { job } => {
                if let Some(jobs::Job {
                    state:
                        jobs::State::Ended {
                            outcome,
                            torn_down: true,
                            ..
                        },
                    ..
                }) = self.jobs.get(job)
                    && self.leases.all_ended()
                {
                    let agents = self
                        .agents
                        .iter()
                        .map(|(&id, work)| work.outcome(id))
                        .collect();
                    let outcome = Outcome {
                        agents,
                        result: outcome.clone(),
                    };
                    actions.push(Action::Finish { outcome });
                    self.phase = Phase::Finished;
                }
            }
            Phase::Finished | Phase::Abandoned => {}
        }
    }
}
