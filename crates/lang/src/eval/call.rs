//! Functions: a `def` makes one when it runs, and a call runs its body (spec.md, "Functions"
//! and "Function definitions"), by the rules in [`crate::eval`]'s doc.

use alloc::format;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::mem;

use super::{Evaluation, Locals, MAX_CALLS, Scope, assign};
use crate::builtins::{Call, called, lacks, origin, place};
use crate::problem::Fault;
use crate::syntax::{Def, Inside, Misfit, Simple, Step};
use crate::value::{FileId, Function, Kind, Span, Value};
use crate::{Loader, MAX_NESTING};

impl<L: Loader> Evaluation<'_, '_, L> {
    /// The function that `def` makes, run at `module`'s top level, where its defaults are
    /// evaluated, each refused unless it fits its parameter's annotation.
    pub(super) fn define(&mut self, def: &Rc<Def>, module: FileId) -> Result<Value, Fault> {
        let scope = Scope::Top(module);
        let mut defaults = Vec::new();
        for param in &def.params {
            let Some(default) = &param.default else {
                defaults.push(None);
                continue;
            };
            let value = self.eval(&scope, default)?;
            // Where the `def` runs, so that a default that doesn't fit is refused though no call
            // takes it, as Bazel checks it:
            // https://github.com/bazelbuild/bazel/blob/a765dc177f48efb4c9e1b980609b838c9ba27f42/src/main/java/net/starlark/java/eval/Eval.java#L260-L270
            if let Some(annotation) = &param.annotation {
                match annotation.misfit(&value) {
                    None => {}
                    Some(Misfit::Whole) => {
                        let message = format!(
                            "`{}` is {}, and can't default to {}: give {}",
                            param.name.text,
                            annotation.describe(),
                            value.shown(),
                            annotation.describe()
                        );
                        let fault = Fault::new(default.span, message);
                        return Err(origin(fault, "the default", &value));
                    }
                    Some(Misfit::Inside(inside)) => {
                        let whole = format!("`{}`'s default", param.name.text);
                        return Err(refusal_inside(&inside, &whole, default.span));
                    }
                }
            }
            defaults.push(Some(value));
        }
        let function = Function::new(Rc::clone(def), module, defaults, self.budget)?;
        Ok(Value {
            kind: Kind::Function(Rc::new(function)),
            span: def.name.span,
        })
    }

    /// What `call` of `function` gives: what its body returns, or `None`, the body run as deep
    /// as the call's arguments nest, `depth` deep, in the calls that run.
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
        let returned = returned.and_then(|returned| checked_return(def, returned, at));
        returned.map_err(|fault| fault.note(called(def), Some(at)))
    }

    fn steps<'d>(
        &mut self,
        module: FileId,
        locals: &mut Locals<'d>,
        steps: &'d [Step],
    ) -> Result<Option<(Span, Value)>, Fault> {
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
                    return Ok(Some((*keyword, value)));
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
                    if let Some(returned) = self.steps(module, locals, chosen)? {
                        return Ok(Some(returned));
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
    let mut arguments = Vec::new();
    let mut missing = Vec::new();
    for ((param, given), default) in def.params.iter().zip(given).zip(&function.defaults) {
        match (given, default) {
            (Some((_, value)), _) => arguments.push((param, value)),
            // Not checked against its annotation here, as `define` checked it, as Bazel does:
            // https://github.com/bazelbuild/bazel/blob/a765dc177f48efb4c9e1b980609b838c9ba27f42/src/main/java/net/starlark/java/eval/StarlarkFunction.java#L626-L628
            (None, Some(default)) => {
                locals.insert(&param.name.text, Some(default.clone()));
            }
            (None, None) => missing.push(param.name.text.as_str()),
        }
    }
    if !missing.is_empty() {
        return Err(lacks(&def.name.text, &missing, call.at));
    }
    // Each argument is checked once all parameters are bound, as starlark-rust checks them:
    // https://github.com/facebook/starlark-rust/blob/85e33edead0189db7a8312005ae8175a30bf155c/starlark/src/eval/compiler/def.rs#L796-L815
    for (param, value) in arguments {
        if let Some(annotation) = &param.annotation {
            match annotation.misfit(&value) {
                None => {}
                Some(Misfit::Whole) => {
                    let message = format!(
                        "`{}`'s `{}` can't be {}: give {}",
                        def.name.text,
                        param.name.text,
                        value.shown(),
                        annotation.describe()
                    );
                    return Err(origin(Fault::new(call.at, message), "the argument", &value));
                }
                Some(Misfit::Inside(inside)) => {
                    let whole = format!("`{}`'s `{}`", def.name.text, param.name.text);
                    return Err(refusal_inside(&inside, &whole, call.at));
                }
            }
        }
        locals.insert(&param.name.text, Some(value));
    }
    Ok(locals)
}

/// What a call of `def` at `at` gives: `returned`, a `return`'s keyword and its value, or else
/// `None`, refused unless it fits the type annotated after `->`.
// A value that doesn't fit is refused at its `return`, as starlark-rust refuses it there, or else
// at the function's name, where starlark-rust refuses it at the end of the body's last
// statement, a span that the subset's steps don't hold:
// https://github.com/facebook/starlark-rust/blob/85e33edead0189db7a8312005ae8175a30bf155c/starlark/src/eval/bc/compiler/stmt.rs#L185-L196
// https://github.com/facebook/starlark-rust/blob/85e33edead0189db7a8312005ae8175a30bf155c/starlark/src/eval/bc/compiler/stmt.rs#L267-L279
fn checked_return(def: &Def, returned: Option<(Span, Value)>, at: Span) -> Result<Value, Fault> {
    let none = Value {
        kind: Kind::None,
        span: at,
    };
    let (end, value) = returned.unwrap_or((def.name.span, none));
    if let Some(returns) = &def.returns {
        match returns.misfit(&value) {
            None => {}
            Some(Misfit::Whole) => {
                let message = format!(
                    "`{}` can't return {}: it returns {}",
                    def.name.text,
                    value.shown(),
                    returns.describe()
                );
                return Err(Fault::new(end, message));
            }
            Some(Misfit::Inside(inside)) => {
                let whole = format!("what `{}` returns", def.name.text);
                return Err(refusal_inside(&inside, &whole, end));
            }
        }
    }
    Ok(value)
}

/// The refusal, at `at`, of the value that `inside` finds in the one that `whole` names, with a
/// note on where that value is made.
pub(super) fn refusal_inside(inside: &Inside, whole: &str, at: Span) -> Fault {
    let mut named = format!("{} ", inside.part.of());
    for part in &inside.outer {
        named.push_str(part.of());
        named.push(' ');
    }
    let message = format!(
        "{named}{whole} can't be {}: give {}",
        inside.found.shown(),
        inside.fits
    );
    origin(Fault::new(at, message), inside.part.the(), &inside.found)
}
