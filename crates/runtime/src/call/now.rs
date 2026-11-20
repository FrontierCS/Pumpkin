//! A now function's call: its caller's arguments read against the types the IR records, then its
//! body evaluated as the call arrives, before the core takes the call.

use std::collections::BTreeMap;

use decision::{CallResult, Effect, Evaluated, Unevaluated};
use schema::ir::{Name, argument};
use schema::workflow::NowFunction;

use crate::body::{Bodies, RunState};

/// What the body of `function` gave, called with its caller's `arguments`, each read against its
/// parameter's type, through `bodies`.
pub(crate) fn evaluate(
    function: &NowFunction,
    arguments: &BTreeMap<Name, String>,
    bodies: &dyn Bodies,
) -> Result<Evaluated, Unevaluated> {
    let parameters = &function.body.arguments;
    let arguments =
        argument::read(&function.name, parameters, arguments).map_err(Unevaluated::Unread)?;
    let evaluate = || {
        let mut proposed = Proposed(Vec::new());
        let returned = bodies.now(&function.body, &arguments, &mut proposed)?;
        Ok(Evaluated {
            effects: proposed.0,
            returned: CallResult::returned_by(returned)?,
        })
    };
    // At the top of a stack of the evaluator's size, as every other evaluation is: the run loop's
    // thread is the main one, whose stack is what the process's limit makes it, below the loop's
    // frames.
    match stdx::on_stack("now", bodies.stack_bytes(), evaluate) {
        Ok(evaluated) => evaluated.map_err(Unevaluated::Failed),
        Err(error) => Err(Unevaluated::Failed(format!(
            "the body's thread didn't start: {error}"
        ))),
    }
}

// The effects a body proposes, in order.
struct Proposed(Vec<Effect>);

impl RunState for Proposed {
    fn set_submission(&mut self) {
        self.0.push(Effect::SetSubmission);
    }

    fn end_work(&mut self) {
        self.0.push(Effect::EndWork);
    }
}
