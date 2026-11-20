//! A call of a body, a job function's or the final call's: the call starts its target, calls the
//! body on it ([`body::evaluate`](crate::body::evaluate)), and reports what the body returned.

use std::time::Duration;

use decision::{CallResult, JobId};
use schema::ir::AbsolutePath;
use schema::sandbox::OpPayloadLimit;
use schema::sandbox::op::OpReply;
use schema::workflow::Holder;

use crate::Host;
use crate::body::{self, Bodies, Ended, Of};
use crate::child::{Child, End};
use crate::record::Stored;

/// A call of a body, as its thread drives it.
pub(crate) struct Call<'r> {
    pub(crate) job: JobId,
    pub(crate) of: Of<'r>,
    pub(crate) payload: Stored,
    pub(crate) staged: Vec<(AbsolutePath, Stored)>,
    /// The longest the body may drive the target.
    pub(crate) timeout: Duration,
}

pub(crate) fn run<H: Host>(call: Call<'_>, bodies: &dyn Bodies, child: &mut Child<'_, H, OpReply>) {
    let Call {
        job,
        of,
        payload,
        staged,
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
        staged,
        timeout,
    };
    crate::child::run(child, ops, |child, session| {
        let end = match body::evaluate(bodies, &call, child, session) {
            Ended::Returned(returned) => CallResult::returned_by(returned),
            Ended::Failed(reason) => Err(reason),
            Ended::TimedOut => Err("the call's timeout passed".to_owned()),
            Ended::Stopped => Err("the call was stopped".to_owned()),
        };
        End::Call(job, end)
    });
}
