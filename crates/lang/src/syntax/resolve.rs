//! The checks of a module's names before it runs (spec.md, "Name binding and variables"), which
//! report every problem they find, each at its place:
//! - each name a module uses is bound in the block it is used in, or in one around it: a
//!   comprehension's, by its `for`s; a function's, by its parameters and assignments; the top
//!   level's, by `def`s and assignments; the file's, by `load`s; or it is predeclared;
//! - a global is bound once, a name is loaded once, and no name is both;
//! - a `load` names a global by its name, which doesn't start with `_` (spec.md, "Load
//!   statements");
//! - every `load` comes before the file's other statements but strings, such as a docstring, so
//!   that the names it binds are bound before any of them runs;
//! - an entry file defines no function, as a Bazel BUILD file defines none, so that the functions
//!   that make a workflow are a library's, though its expressions may use the rest of the
//!   language, comprehensions and builtins among them.

use alloc::collections::btree_map::Entry;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::{
    Args, Clause, Comprehension, Expr, ExprKind, Load, Module, Name, Simple, Statement, Step,
};
use crate::builtins;
use crate::problem::{Fault, refusal};
use crate::value::Span;

/// The problems with `module`'s names, in the order of their places; `entry` says whether the
/// module is the entry file's.
pub(crate) fn resolve(module: &Module, entry: bool) -> Vec<Fault> {
    let mut faults = Vec::new();
    let bound = bindings(module, entry, &mut faults);
    let mut uses = Uses {
        bound: &bound,
        blocks: Vec::new(),
        faults: &mut faults,
    };
    for statement in &module.statements {
        match statement {
            Statement::Simple(simple) => uses.simple(simple),
            Statement::Def(def) => {
                // Defaults are evaluated at the top level, where the `def` runs.
                for param in &def.params {
                    if let Some(default) = &param.default {
                        uses.expr(default);
                    }
                }
                uses.blocks
                    .push(def.locals.iter().map(String::as_str).collect());
                uses.steps(&def.body);
                uses.blocks.pop();
            }
        }
    }
    faults.sort_by_key(|fault| fault.at().start);
    faults
}

/// The names `module`, the entry's or not, binds by its loads and at its top level, each with
/// where it is first bound, adding to `faults` the problems with its loads and its bindings.
fn bindings<'m>(
    module: &'m Module,
    entry: bool,
    faults: &mut Vec<Fault>,
) -> BTreeMap<&'m str, (Binding, Span)> {
    let mut bound = BTreeMap::new();
    let docstring = |statement: &&Statement| {
        matches!(
            statement,
            Statement::Simple(Simple::Expr(Expr {
                kind: ExprKind::Str(_),
                ..
            }))
        )
    };
    let other = (module
        .statements
        .iter()
        .find(|statement| !docstring(statement)))
    .map(Statement::span);
    for load in &module.loads {
        if let Some(other) = other
            && other.start < load.keyword.start
        {
            let message = refusal(
                "a `load` after other statements",
                "put every `load` at the top of the file",
            );
            let fault = Fault::new(load.keyword, message);
            faults.push(fault.note("the first other statement is at", Some(other)));
        }
        for name in &load.names {
            faults.extend(loadable(load, &name.remote));
            faults.extend(bind(&mut bound, &name.local, Binding::Loaded));
        }
    }
    for name in module.globals() {
        faults.extend(bind(&mut bound, name, Binding::Global));
    }
    if entry {
        for statement in &module.statements {
            if let Statement::Def(def) = statement {
                let message = refusal(
                    "a function defined in an entry file",
                    "define it in a library, and load it",
                );
                faults.push(Fault::new(def.keyword, message));
            }
        }
    }
    bound
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Binding {
    Loaded,
    Global,
}

/// Binds `name`, or returns the error that the file has bound it already.
fn bind<'m>(
    bound: &mut BTreeMap<&'m str, (Binding, Span)>,
    name: &'m Name,
    binding: Binding,
) -> Option<Fault> {
    let earlier = match bound.entry(&name.text) {
        Entry::Vacant(entry) => {
            entry.insert((binding, name.span));
            return None;
        }
        Entry::Occupied(entry) => *entry.get(),
    };
    let (at, name) = (name.span, &name.text);
    let (message, note) = match (earlier.0, binding) {
        (Binding::Global, Binding::Global) => (
            format!("`{name}` is bound already: bind each global once"),
            "it is first bound at",
        ),
        (Binding::Loaded, Binding::Loaded) => (
            format!("`{name}` is loaded already: load each name once"),
            "it is first loaded at",
        ),
        (Binding::Loaded, Binding::Global) => (
            format!("`{name}` is loaded, so this file can't bind it: bind another name"),
            "it is loaded at",
        ),
        (Binding::Global, Binding::Loaded) => (
            format!(
                "`{name}` is bound in this file, so it can't be loaded: load it as another \
                 name, as `y = \"{name}\"`"
            ),
            "it is bound at",
        ),
    };
    Some(Fault::new(at, message).note(note, Some(earlier.1)))
}

/// The error of `remote`, a name that `load` loads, if it can't name a global that other files
/// may load.
fn loadable(load: &Load, remote: &Name) -> Option<Fault> {
    let text = &remote.text;
    let message = if !super::is_name(text) {
        format!("`{text}` is no name: load a global by its name, as `\"x\"`")
    } else if text.starts_with('_') {
        format!(
            "`{text}` is private to `{}`: load a name that doesn't start with `_`",
            load.label
        )
    } else {
        return None;
    };
    Some(Fault::new(remote.span, message))
}

/// The check that each name a module uses is bound, in the blocks it is in.
struct Uses<'m, 'f> {
    /// The names the module binds at its top level and by its loads.
    bound: &'f BTreeMap<&'m str, (Binding, Span)>,
    /// The names each local block around the use binds, innermost last: a function's, then
    /// comprehensions'.
    blocks: Vec<BTreeSet<&'m str>>,
    faults: &'f mut Vec<Fault>,
}

impl<'m> Uses<'m, '_> {
    fn simple(&mut self, simple: &'m Simple) {
        match simple {
            Simple::Assign { value: expr, .. } | Simple::Expr(expr) => self.expr(expr),
            Simple::Pass(_) => {}
        }
    }

    fn steps(&mut self, steps: &'m [Step]) {
        for step in steps {
            match step {
                Step::Simple(simple) => self.simple(simple),
                Step::Return { value, .. } => {
                    if let Some(value) = value {
                        self.expr(value);
                    }
                }
                Step::If {
                    branches,
                    otherwise,
                } => {
                    for branch in branches {
                        self.expr(&branch.condition);
                        self.steps(&branch.steps);
                    }
                    self.steps(otherwise);
                }
            }
        }
    }

    fn expr(&mut self, expr: &'m Expr) {
        match &expr.kind {
            ExprKind::Name(name) => self.used(name, expr.span),
            ExprKind::Int(_) | ExprKind::Str(_) => {}
            ExprKind::List(items) | ExprKind::Tuple(items) => {
                for item in items {
                    self.expr(item);
                }
            }
            ExprKind::Dict(entries) => {
                for (key, value) in entries {
                    self.expr(key);
                    self.expr(value);
                }
            }
            ExprKind::ListComp {
                body,
                comprehension,
            } => self.comprehension(comprehension, &[body]),
            ExprKind::DictComp {
                key,
                value,
                comprehension,
            } => self.comprehension(comprehension, &[key, value]),
            ExprKind::Call {
                callee: on, args, ..
            }
            | ExprKind::Method {
                receiver: on, args, ..
            } => {
                self.expr(on);
                self.args(args);
            }
            ExprKind::Index { object, index } => {
                self.expr(object);
                self.expr(index);
            }
            ExprKind::Unary { operand, .. } => self.expr(operand),
            ExprKind::Logic { first, rest, .. } => {
                self.expr(first);
                for operand in rest {
                    self.expr(operand);
                }
            }
            ExprKind::Binary { first, rest } => {
                self.expr(first);
                for (_, _, operand) in rest {
                    self.expr(operand);
                }
            }
            ExprKind::Conditional {
                then,
                condition,
                otherwise,
            } => {
                self.expr(then);
                self.expr(condition);
                self.expr(otherwise);
            }
        }
    }

    fn args(&mut self, args: &'m Args) {
        for arg in &args.positional {
            self.expr(arg);
        }
        for (_, arg) in &args.keyword {
            self.expr(arg);
        }
        if let Some(kwargs) = &args.kwargs {
            self.expr(kwargs);
        }
    }

    /// Checks `comprehension`, whose first iterable is in the block around it, and the rest of
    /// whose clauses, and `body`, are in its own block.
    fn comprehension(&mut self, comprehension: &'m Comprehension, body: &[&'m Expr]) {
        self.expr(&comprehension.iterable);
        let names = comprehension
            .names()
            .into_iter()
            .map(|name| name.text.as_str());
        self.blocks.push(names.collect());
        for clause in &comprehension.clauses {
            match clause {
                Clause::For { iterable: expr, .. } | Clause::If(expr) => {
                    self.expr(expr);
                }
            }
        }
        for expr in body {
            self.expr(expr);
        }
        self.blocks.pop();
    }

    /// Reports `name`, used at `span`, as an error unless a block it is in, the module, a load or
    /// the predeclared names bind it; a name the subset leaves out is refused.
    fn used(&mut self, name: &str, span: Span) {
        let local = self.blocks.iter().any(|block| block.contains(name));
        if local || self.bound.contains_key(name) || builtins::predeclared(name).is_some() {
            return;
        }
        let message = builtins::left_out(name).unwrap_or_else(|| {
            format!("`{name}` is not defined: bind it in this file, or load it")
        });
        self.faults.push(Fault::new(span, message));
    }
}
