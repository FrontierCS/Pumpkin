//! A runner belongs to one invocation of its callback. The callback runs on the same evaluator
//! with its usual step, stack and byte limits.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroU64;

use schema::ir;

use super::body::{Host, Target, absolute, command_argv};
use super::{Evaluation, Purpose};
use crate::builtins::{Call, place, string};
use crate::problem::Fault;
use crate::syntax::Name;
use crate::value::{Hosted, Kind, Span, Value};
use crate::{Loader, json};

#[derive(Clone, Copy)]
pub(crate) enum RunnerMethod {
    Get,
    Put,
    Run,
}

impl<L: Loader> Evaluation<'_, '_, L> {
    fn runner_target(&mut self) -> &mut dyn Target {
        let Purpose::Call(Host::Job(target) | Host::Final(target, _)) = &mut self.purpose else {
            unreachable!("only a job or final body receives a Target on which to open a runner")
        };
        *target
    }

    pub(super) fn with_runner(&mut self, call: &Call<'_>, depth: usize) -> Result<Value, Fault> {
        let at = call.at;
        let [grant, callback, arguments @ ..] = call.positional.as_slice() else {
            return Err(Fault::new(
                at,
                "`with_runner` needs a session name and a callback",
            ));
        };
        if call.keywords().next().is_some() {
            return Err(Fault::new(
                at,
                "`with_runner` takes its arguments by position",
            ));
        }
        let grant = string("the session name", grant.0, &grant.1)?;
        let grant = ir::Name::try_from(String::from(grant)).map_err(|e| Fault::new(at, e))?;
        let Kind::Function(function) = &callback.1.kind else {
            return Err(Fault::new(
                callback.0,
                "the runner's callback must be a function",
            ));
        };
        let lease = self
            .runner_target()
            .runner_open(&grant)
            .map_err(|why| Fault::new(at, why))?;
        self.runners.insert(lease, grant.clone());
        let runner = Value {
            kind: Kind::Hosted(Hosted::Runner(lease)),
            span: at,
        };
        let positional = core::iter::once((at, runner))
            .chain(arguments.iter().cloned())
            .collect();
        let callback = Call::of(at, positional, Vec::new(), call.budget);
        let returned = self.function(function, &callback, depth);
        self.runners.remove(&lease);
        match (returned, self.runner_target().runner_close(&grant, lease)) {
            (returned, Ok(())) => returned,
            (Ok(_), Err(why)) => Err(Fault::new(at, why)),
            (Err(fault), Err(why)) => {
                Err(fault.note(format!("closing the runner also failed: {why}"), None))
            }
        }
    }

    pub(super) fn runner_method(
        &mut self,
        lease: u64,
        method: RunnerMethod,
        name: &Name,
        call: &Call<'_>,
    ) -> Result<Value, Fault> {
        let at = call.at;
        let grant = self.runners.get(&lease).cloned().ok_or_else(|| {
            Fault::new(
                at,
                "the runner's callback has ended: use a runner within its callback",
            )
        })?;
        match method {
            RunnerMethod::Get => {
                let (from, to, container) = copy(name, ["from", "to"], call)?;
                let from = absolute("the runner's source path", &from)?;
                let to = absolute("the target's destination directory", &to)?;
                self.runner_target()
                    .runner_get(&grant, lease, from, to, container)
                    .map_err(|why| Fault::new(at, why))?;
            }
            RunnerMethod::Put => {
                let (tree, to, container) = copy(name, ["tree", "to"], call)?;
                let tree = absolute("the target's source directory", &tree)?;
                let to = absolute("the runner's destination directory", &to)?;
                self.runner_target()
                    .runner_put(&grant, lease, tree, to, container)
                    .map_err(|why| Fault::new(at, why))?;
            }
            RunnerMethod::Run => {
                let params = ["argv", "timeout_seconds", "companion"];
                let given = place(&name.text, &params, 1, call)?;
                let Some(argv) = &given[0] else {
                    return Err(Fault::new(at, "`run` needs argv"));
                };
                let argv = command_argv(name, argv.span, argv)?;
                let timeout_seconds = given[1].as_ref().map(seconds).transpose()?;
                let container = given[2].as_ref().map(companion).transpose()?;
                let output = self
                    .runner_target()
                    .runner_run(&grant, lease, argv, timeout_seconds, container)
                    .map_err(|why| Fault::new(at, why))?;
                return json::value_of(&serde_json::json!(output), at, call.budget);
            }
        }
        Ok(Value {
            kind: Kind::None,
            span: at,
        })
    }
}

/// A copy's two paths, `paths` by name, given by position or by name, and the runner's companion
/// to copy to or from, if `companion` names one: what `get` and `put` take.
fn copy(
    name: &Name,
    paths: [&str; 2],
    call: &Call<'_>,
) -> Result<(Argument, Argument, Option<ir::CompanionName>), Fault> {
    let given = place(&name.text, &[paths[0], paths[1], "companion"], 2, call)?;
    let path = |index: usize| {
        let value = given[index].clone().ok_or_else(|| {
            Fault::new(call.at, format!("`{}` needs `{}`", name.text, paths[index]))
        })?;
        Ok::<_, Fault>((value.span, value))
    };
    let container = given[2].as_ref().map(companion).transpose()?;
    Ok((path(0)?, path(1)?, container))
}

/// An argument and where it was given.
type Argument = (Span, Value);

fn companion(value: &Value) -> Result<ir::CompanionName, Fault> {
    let name = string("the companion's name", value.span, value)?;
    ir::CompanionName::try_from(String::from(name)).map_err(|why| Fault::new(value.span, why))
}

fn seconds(value: &Value) -> Result<NonZeroU64, Fault> {
    let seconds = match value.kind {
        Kind::Int(n) => u64::try_from(n).ok().and_then(NonZeroU64::new),
        _ => None,
    };
    seconds.ok_or_else(|| Fault::new(value.span, "timeout_seconds must be a positive integer"))
}
