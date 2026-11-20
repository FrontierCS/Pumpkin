//! A runner belongs to one invocation of its callback. The callback runs on the same evaluator
//! with its usual step, stack and byte limits. Only the host translates a grant into a lease.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroU64;

use schema::ir;
use schema::sandbox::op::{CommandEnd, Last, LeaseStatusReply, SessionOp, TransferEnd};

use super::body::{Host, absolute, command_argv};
use super::{Evaluation, Purpose};
use crate::builtins::{Call, Takes, place, positional, string};
use crate::problem::Fault;
use crate::syntax::Name;
use crate::value::{Hosted, Kind, Span, Value};
use crate::{Loader, json};

#[derive(Clone, Copy)]
pub(crate) enum RunnerMethod {
    Put,
    Run,
}

impl<L: Loader> Evaluation<'_, '_, L> {
    fn operate(
        &mut self,
        grant: &ir::Name,
        op: SessionOp,
        at: Span,
    ) -> Result<LeaseStatusReply, Fault> {
        let Purpose::Call(Host::Job(target) | Host::Final(target, _)) = &mut self.purpose else {
            unreachable!("only a job or final body receives a Target on which to open a runner")
        };
        target
            .runner(grant, op)
            .map_err(|message| Fault::new(at, message))
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
        let opened = self.operate(
            &grant,
            SessionOp::Open {
                instance_seconds: None,
            },
            at,
        )?;
        let lease = opened.lease;
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
        self.operate(&grant, SessionOp::Close { lease }, at)?;
        returned
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
            RunnerMethod::Put => {
                let [tree, to] = positional::<2>(&name.text, Takes::exactly(2), call)?;
                let tree = absolute("the target's source directory", tree)?;
                let to = absolute("the runner's destination directory", to)?;
                let op = SessionOp::Put {
                    lease,
                    tree,
                    to,
                    replace: false,
                };
                match self.operate(&grant, op, at)?.last {
                    Some(Last::Transfer(transfer)) => match transfer.end {
                        Some(TransferEnd::Moved { .. }) => Ok(Value {
                            kind: Kind::None,
                            span: at,
                        }),
                        Some(TransferEnd::Failed(reason)) => Err(Fault::new(at, reason)),
                        None => Err(Fault::new(at, "the runner's copy has no receipt")),
                    },
                    _ => Err(Fault::new(at, "the runner's copy has no receipt")),
                }
            }
            RunnerMethod::Run => {
                let given = place(&name.text, &["argv", "timeout_seconds"], 1, call)?;
                let Some(argv) = &given[0] else {
                    return Err(Fault::new(at, "`run` needs argv"));
                };
                let argv = command_argv(name, argv.span, argv)?;
                let timeout_seconds = given[1].as_ref().map(seconds).transpose()?;
                let op = SessionOp::Run {
                    lease,
                    argv,
                    timeout_seconds,
                };
                let Some(Last::Command(command)) = self.operate(&grant, op, at)?.last else {
                    return Err(Fault::new(at, "the runner's command has no receipt"));
                };
                let end = match command.end {
                    Some(CommandEnd::Unstarted) => {
                        return Err(Fault::new(
                            at,
                            format!("the command did not start: {}", command.stderr),
                        ));
                    }
                    Some(end) => end,
                    None => return Err(Fault::new(at, "the runner's command has no receipt")),
                };
                let result = serde_json::json!({
                    "end": end,
                    "stdout_tail": command.stdout,
                    "stderr_tail": command.stderr,
                });
                json::value_of(&result, at, call.budget)
            }
        }
    }
}

fn seconds(value: &Value) -> Result<NonZeroU64, Fault> {
    let seconds = match value.kind {
        Kind::Int(n) => u64::try_from(n).ok().and_then(NonZeroU64::new),
        _ => None,
    };
    seconds.ok_or_else(|| Fault::new(value.span, "timeout_seconds must be a positive integer"))
}
