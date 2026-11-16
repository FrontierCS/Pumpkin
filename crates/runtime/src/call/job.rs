//! A call of a body, a job function's or the final call's: the call starts its target, calls the
//! body on it ([`body::evaluate`](crate::body::evaluate)), and reports what the body returned.

use std::time::Duration;

use decision::{CallResult, Event, JobId};
use schema::sandbox::OpPayloadLimit;
use schema::sandbox::op::OpReply;
use schema::workflow::Holder;

use crate::Host;
use crate::body::{self, Bodies, Ended, Of};
use crate::child::Child;
use crate::record::Stored;

/// A call of a body, as its thread drives it.
pub(crate) struct Call<'r> {
    pub(crate) job: JobId,
    pub(crate) of: Of<'r>,
    pub(crate) payload: Stored,
    /// The longest the body may drive the target.
    pub(crate) timeout: Duration,
}

pub(crate) fn run<H: Host>(call: Call<'_>, bodies: &dyn Bodies, child: &mut Child<'_, H, OpReply>) {
    let Call {
        job,
        of,
        payload,
        timeout,
    } = call;
    let target = of.target();
    let workflow = child.sandboxes.workflow();
    let holder = Holder::Target(target);
    // A target's ops carry no payload: it holds sessions alone.
    let ops = (workflow.sessions())
        .any(|(_, grant)| grant.holder == holder)
        .then_some(OpPayloadLimit::NONE);
    let call = body::Call {
        job,
        target: workflow.target(target),
        of: &of,
        payload,
        timeout,
    };
    crate::child::run(child, ops, |child, session| {
        let failed = |reason: String| Event::CallFailed { job, reason };
        match body::evaluate(bodies, &call, child, session) {
            Ended::Returned(returned) => match CallResult::returned_by(returned) {
                Ok(result) => Event::CallReturned { job, result },
                Err(reason) => failed(reason),
            },
            Ended::Failed(reason) => failed(reason),
            Ended::TimedOut => failed("the call's timeout passed".to_owned()),
            Ended::Stopped => failed("the call was stopped".to_owned()),
        }
    });
}
