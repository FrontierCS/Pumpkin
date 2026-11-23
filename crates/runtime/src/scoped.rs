//! A body waits in its own job inbox for the numbered work its session grant admitted. The
//! driver's thread registers the waiter before starting that work, and wakes it after recording
//! the event that finished it. A sandbox sharing the job can have a gateway op outstanding too.

use decision::{Caller, Core, Event, JobId, LeaseId};
use schema::sandbox::op::{Last, LeaseState, LeaseStatusReply, OpReply};

use crate::child::Message;
use crate::{Host, Runtime, relay};

#[derive(Clone, Copy)]
pub(super) enum Waiting {
    Open,
    Work,
    Close,
}

pub(super) struct Waiter {
    job: JobId,
    waiting: Waiting,
    number: Option<u64>,
}

impl<'r, H: Host + 'r> Runtime<'r, '_, H> {
    pub(super) fn watch(
        &mut self,
        core: &Core<'_>,
        job: JobId,
        waiting: Waiting,
        reply: OpReply,
    ) -> Result<(), String> {
        let id = match reply {
            OpReply::Refused { message, .. } => {
                self.answer_body(job, Err(message));
                return Ok(());
            }
            OpReply::Lease(accepted) => LeaseId(accepted.lease),
            OpReply::Ops(_) | OpReply::Call(_) | OpReply::Status(_) | OpReply::LeaseStatus(_) => {
                return Err(
                    "the core replied to a body session operation with another kind of reply"
                        .to_owned(),
                );
            }
        };
        let status = core
            .held_lease(Caller::Job(job), id)
            .ok_or_else(|| format!("the accepted lease {} has no such holder", id.0))?;
        let number = match &status.last {
            Some(Last::Command(last)) => Some(last.command),
            Some(Last::Transfer(last)) => Some(last.transfer),
            None => None,
        };
        let waiter = Waiter {
            job,
            waiting,
            number,
        };
        if let Some(result) = waiter.completed(status) {
            self.answer_body(job, result);
        } else {
            self.waiting.insert(id, waiter);
        }
        Ok(())
    }

    pub(super) fn receipt(
        &mut self,
        core: &Core<'_>,
        event: &Event,
    ) -> Option<(JobId, Result<LeaseStatusReply, String>)> {
        let lease = match event {
            Event::LeaseReady { lease }
            | Event::LeaseEnded { lease }
            | Event::LeaseFailed { lease, .. }
            | Event::CommandEnded { lease, .. }
            | Event::TransferEnded { lease, .. } => *lease,
            _ => return None,
        };
        let waiter = self.waiting.get(&lease)?;
        let job = waiter.job;
        let result = match event {
            Event::LeaseFailed { reason, .. } => Some(Err(reason.clone())),
            _ => core
                .held_lease(Caller::Job(job), lease)
                .and_then(|status| waiter.completed(status)),
        };
        let receipt = result?;
        self.waiting.remove(&lease);
        Some((job, receipt))
    }

    pub(super) fn answer_body(&mut self, job: JobId, result: Result<LeaseStatusReply, String>) {
        let _ = self
            .call(job)
            .orders
            .send(Message::Order(relay::Order::Body(result)));
    }
}

impl Waiter {
    fn completed(&self, status: LeaseStatusReply) -> Option<Result<LeaseStatusReply, String>> {
        let ready = match self.waiting {
            Waiting::Open => status.state == LeaseState::Ready,
            Waiting::Work => {
                status.state == LeaseState::Ready
                    && match &status.last {
                        Some(Last::Command(last)) => {
                            Some(last.command) == self.number && last.end.is_some()
                        }
                        Some(Last::Transfer(last)) => {
                            Some(last.transfer) == self.number && last.end.is_some()
                        }
                        None => false,
                    }
            }
            Waiting::Close => status.state == LeaseState::Closed,
        };
        if ready {
            Some(Ok(status))
        } else if matches!(
            status.state,
            LeaseState::Closed | LeaseState::Expired | LeaseState::Failed
        ) {
            Some(Err(status.failure.unwrap_or_else(|| {
                format!("lease {} ended ({:?})", status.lease, status.state)
            })))
        } else {
            None
        }
    }
}
