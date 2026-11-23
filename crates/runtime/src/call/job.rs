//! A call of a body, a job function's or the final call's: the call starts its target, calls the
//! body on it ([`body::evaluate`](crate::body::evaluate)), and reports what the body returned.

use std::time::Duration;

use decision::{CallResult, JobId};
use gateway::Remote;
use schema::sandbox::OpPayloadLimit;
use schema::workflow::Holder;

use crate::Host;
use crate::body::{self, Bodies, Ended, Of};
use crate::child::{Child, End};
use crate::record::Stored;

/// A call of a body, as its thread drives it.
pub(crate) struct Call<'r> {
    pub(crate) job: JobId,
    pub(crate) of: Of<'r>,
    pub(crate) inputs: Vec<Stored>,
    /// The longest the body may drive the target.
    pub(crate) timeout: Duration,
}

pub(crate) fn run<H: Host>(
    call: Call<'_>,
    bodies: &dyn Bodies,
    child: &mut Child<'_, H, crate::relay::Order>,
) {
    let holder = Holder::Target(call.of.target());
    // A target's ops carry no payload: it holds sessions alone.
    let ops = child
        .sandboxes
        .workflow()
        .sessions()
        .any(|(_, grant)| grant.holder == holder)
        .then_some(OpPayloadLimit::NONE);
    crate::child::run(child, ops, |child, session| {
        evaluate(call, bodies, child, session, &mut body::Stdin::default())
    });
}

pub(crate) fn evaluate<H: Host>(
    call: Call<'_>,
    bodies: &dyn Bodies,
    child: &mut Child<'_, H, crate::relay::Order>,
    session: &mut Remote,
    stdin: &mut body::Stdin,
) -> End {
    let Call {
        job,
        of,
        inputs,
        timeout,
    } = call;
    let target = of.target();
    let workflow = child.sandboxes.workflow();
    let call = body::Call {
        job,
        target: workflow.target(target),
        of: &of,
        inputs,
        timeout,
    };
    let end = match body::evaluate(bodies, &call, child, session, stdin) {
        Ended::Returned(returned) => CallResult::returned_by(returned),
        Ended::Failed(reason) => Err(reason),
        Ended::TimedOut => Err("the call's timeout passed".to_owned()),
        Ended::Stopped => Err("the call was stopped".to_owned()),
    };
    End::Call(job, end)
}
