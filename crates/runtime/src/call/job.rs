//! A job function's call: the call starts the function's target, calls its body on it
//! ([`body::evaluate`](crate::body::evaluate)), and reports what the body returned.

use std::collections::BTreeMap;
use std::time::Duration;

use decision::{Event, JobId, JudgeResult};
use schema::ir::{ArgumentValue, Name};
use schema::sandbox::OpPayloadLimit;
use schema::sandbox::op::OpReply;
use schema::workflow::{Holder, JobFunction};

use crate::Host;
use crate::body::{self, Bodies, Ended};
use crate::child::Child;
use crate::record::Stored;

/// A job function's call, as its thread drives it.
pub(crate) struct Call<'r> {
    pub(crate) job: JobId,
    pub(crate) function: &'r JobFunction,
    pub(crate) payload: Stored,
    pub(crate) arguments: BTreeMap<Name, ArgumentValue>,
    /// The longest the body may drive the target.
    pub(crate) timeout: Duration,
}

pub(crate) fn run<H: Host>(call: Call<'_>, bodies: &dyn Bodies, child: &mut Child<'_, H, OpReply>) {
    let Call {
        job,
        function,
        payload,
        arguments,
        timeout,
    } = call;
    let workflow = child.sandboxes.workflow();
    let holder = Holder::Target(function.target);
    // A target's ops carry no payload: it holds sessions alone.
    let ops = (workflow.sessions())
        .any(|(_, grant)| grant.holder == holder)
        .then_some(OpPayloadLimit::NONE);
    let call = body::Call {
        job,
        target: workflow.target(function.target),
        body: &function.body,
        keywords: &function.keywords,
        arguments: &arguments,
        payload,
        timeout,
    };
    crate::child::run(child, ops, |child, session| {
        let failed = |reason: String| Event::CallFailed { job, reason };
        match body::evaluate(bodies, &call, child, session) {
            Ended::Returned(returned) => match JudgeResult::returned_by(returned) {
                Ok(result) => Event::CallReturned { job, result },
                Err(reason) => failed(reason),
            },
            Ended::Failed(reason) => failed(reason),
            Ended::TimedOut => failed("the call's timeout passed".to_owned()),
            Ended::Stopped => failed("the call was stopped".to_owned()),
        }
    });
}
