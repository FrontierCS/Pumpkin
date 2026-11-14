//! Functions: a `def` makes one when it runs, and a call runs its body (spec.md, "Functions"
//! and "Function definitions"), by the rules in [`crate::eval`]'s doc.

use alloc::format;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::mem;

use super::{Evaluation, Locals, MAX_CALLS, Scope, assign};
use crate::builtins::{Call, called, lacks, place};
use crate::problem::Fault;
use crate::syntax::{Def, Simple, Step};
use crate::value::{FileId, Function, Kind, Value};
use crate::{Loader, MAX_NESTING};

impl<L: Loader> Evaluation<'_, L> {
    /// The function that `def` makes, run at `module`'s top level, where its defaults are
    /// evaluated.
    pub(super) fn define(&mut self, def: &Rc<Def>, module: FileId) -> Result<Value, Fault> {
        let scope = Scope::Top(module);
        let defaults = (def.params.iter())
            .map(|param| {
                let default = param.default.as_ref();
                default
                    .map(|default| self.eval(&scope, default))
                    .transpose()
            })
            .collect::<Result<_, _>>()?;
        let function = Function::new(Rc::clone(def), module, defaults, self.budget)?;
        Ok(Value {
            kind: Kind::Function(Rc::new(function)),
            span: def.name.span,
        })
    }

    /// What `call` of `function` gives: what its body returns, or `None`, the body run as deep
    /// as the call's arguments nest, `depth` deep, in the calls that run.
    ///
    /// # Errors
    ///
    /// The function runs already, `MAX_CALLS` run, the body would nest deeper than
    /// `MAX_NESTING` there, the arguments bind no parameters, or the body's error, with a note
    /// that names the call.
    pub(super) fn function(
        &mut self,
        function: &Function,
        call: &Call<'_>,
        depth: usize,
    ) -> Result<Value, Fault> {
        let def = &*function.def;
        let name = &def.name.text;
        let at = call.at;
        if (self.calls.iter()).any(|(running, _)| Rc::ptr_eq(running, &function.def)) {
            let message = format!(
                "this calls `{name}` while it runs, and a Starlark function never calls itself, \
                 even through others: compute the value without calling `{name}` again"
            );
            return Err(Fault::new(at, message));
        }
        if self.calls.len() == MAX_CALLS {
            let message = format!(
                "this call would run {} functions at once: nest calls at most {MAX_CALLS} deep",
                MAX_CALLS + 1
            );
            return Err(Fault::new(at, message));
        }
        let base = self.depth + depth;
        if base + def.depth > MAX_NESTING {
            let message = format!(
                "this call nests `{name}`'s body {} deep, counting the calls it is in: nest \
                 expressions, blocks and calls at most {MAX_NESTING} deep",
                base + def.depth
            );
            return Err(Fault::new(at, message));
        }
        let mut locals = bind(function, call)?;
        self.calls.push((Rc::clone(&function.def), at));
        let caller = mem::replace(&mut self.depth, base);
        let returned = self.steps(function.module, &mut locals, &def.body);
        self.calls.pop();
        self.depth = caller;
        let returned = returned.map_err(|fault| fault.note(called(def), Some(at)))?;
        Ok(returned.unwrap_or(Value {
            kind: Kind::None,
            span: at,
        }))
    }

    /// Runs `steps`, a body of the function in `module`, whose names are `locals`, and gives
    /// what a `return` among them gives, if one runs.
    fn steps<'d>(
        &mut self,
        module: FileId,
        locals: &mut Locals<'d>,
        steps: &'d [Step],
    ) -> Result<Option<Value>, Fault> {
        for step in steps {
            let scope = Scope::Function {
                module,
                locals: &*locals,
            };
            match step {
                Step::Simple(Simple::Assign { target, value }) => {
                    let value = self.eval(&scope, value)?;
                    assign(target, value, &mut |name, value| {
                        locals.insert(name, Some(value));
                    })?;
                }
                Step::Simple(Simple::Expr(expr)) => drop(self.eval(&scope, expr)?),
                Step::Simple(Simple::Pass(_)) => {}
                Step::Return { keyword, value } => {
                    let value = match value {
                        Some(value) => self.eval(&scope, value)?,
                        None => Value {
                            kind: Kind::None,
                            span: *keyword,
                        },
                    };
                    return Ok(Some(value));
                }
                Step::If {
                    branches,
                    otherwise,
                } => {
                    let mut chosen = otherwise.as_slice();
                    for branch in branches {
                        if self.eval(&scope, &branch.condition)?.truth() {
                            chosen = &branch.steps;
                            break;
                        }
                    }
                    if let Some(value) = self.steps(module, locals, chosen)? {
                        return Ok(Some(value));
                    }
                }
            }
        }
        Ok(None)
    }
}

/// The names of `function`'s body as `call` of it begins: its parameters bound to the
/// arguments, or else to their defaults, and the rest unbound.
fn bind<'d>(function: &'d Function, call: &Call<'_>) -> Result<Locals<'d>, Fault> {
    let def = &*function.def;
    let params: Vec<&str> = def.params.iter().map(|p| p.name.text.as_str()).collect();
    let given = place(&def.name.text, &params, def.positional, call)?;
    let mut locals: Locals<'d> = (def.locals.iter())
        .map(|local| (local.as_str(), None))
        .collect();
    let mut missing = Vec::new();
    for ((param, given), default) in def.params.iter().zip(given).zip(&function.defaults) {
        let Some(value) = given.or_else(|| default.clone()) else {
            missing.push(param.name.text.as_str());
            continue;
        };
        locals.insert(&param.name.text, Some(value));
    }
    if missing.is_empty() {
        return Ok(locals);
    }
    Err(lacks(&def.name.text, &missing, call.at))
}
