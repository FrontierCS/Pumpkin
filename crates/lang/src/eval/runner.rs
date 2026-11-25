//! A runner belongs to one invocation of its callback. The callback runs on the same evaluator
//! with its usual step, stack and byte limits.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroU64;

use schema::ir;

use super::body::{Host, Target, absolute, command_argv};
use super::{Evaluation, Purpose};
use crate::builtins::{Call, origin, place, string};
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
                let ([from, to], container) = copy(name, ["from", "to"], call)?;
                let from = absolute("the runner's source path", &from)?;
                let to = absolute("the target's destination directory", &to)?;
                self.runner_target()
                    .runner_get(&grant, lease, container, from, to)
                    .map_err(|why| Fault::new(at, why))?;
            }
            RunnerMethod::Put => {
                let ([tree, to], container) = copy(name, ["tree", "to"], call)?;
                let tree = absolute("the target's source directory", &tree)?;
                let to = absolute("the runner's destination directory", &to)?;
                self.runner_target()
                    .runner_put(&grant, lease, container, tree, to)
                    .map_err(|why| Fault::new(at, why))?;
            }
            RunnerMethod::Run => {
                let params = ["argv", "timeout_seconds", "container"];
                let given = place(&name.text, &params, 1, call)?;
                let Some(argv) = &given[0] else {
                    return Err(Fault::new(at, "`run` needs argv"));
                };
                let argv = command_argv(name, argv.0, &argv.1)?;
                let timeout_seconds = given[1].as_ref().map(seconds).transpose()?;
                let container = container(given[2].as_ref())?;
                let output = self
                    .runner_target()
                    .runner_run(&grant, lease, container, argv, timeout_seconds)
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

/// What `put` and `get`, `name`, take: the two paths that `paths` names, each where it was given,
/// and the container to copy to or from. A path is given by position or by name.
fn copy(
    name: &Name,
    paths: [&str; 2],
    call: &Call<'_>,
) -> Result<([(Span, Value); 2], ir::Container), Fault> {
    let given = place(&name.text, &[paths[0], paths[1], "container"], 2, call)?;
    let path = |index: usize| {
        given[index]
            .clone()
            .ok_or_else(|| Fault::new(call.at, format!("`{}` needs `{}`", name.text, paths[index])))
    };
    Ok(([path(0)?, path(1)?], container(given[2].as_ref())?))
}

/// The runner's container that `argument`, a string, names. Without it, `main`.
fn container(argument: Option<&(Span, Value)>) -> Result<ir::Container, Fault> {
    let Some((at, value)) = argument else {
        return Ok(ir::Container::Main);
    };
    let name = string("a container's name", *at, value)?;
    ir::Container::try_from(String::from(name))
        .map_err(|why| origin(Fault::new(*at, why), "the argument", value))
}

fn seconds((at, value): &(Span, Value)) -> Result<NonZeroU64, Fault> {
    let seconds = match value.kind {
        Kind::Int(n) => u64::try_from(n).ok().and_then(NonZeroU64::new),
        _ => None,
    };
    let refused = || Fault::new(*at, "timeout_seconds must be a positive integer");
    seconds.ok_or_else(|| origin(refused(), "the argument", value))
}
