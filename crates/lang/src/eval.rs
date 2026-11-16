//! The evaluator: runs modules and calls as spec.md's "Module execution" and "Functions" say.
//!
//! A module runs once per evaluation, the first time a file loads it, and every file that loads
//! it shares its globals; a load of a module that is still loading is an error, with the chain of
//! loads. The specification leaves both to the application ("Load statements"). A module's globals
//! exist, unbound, before its first statement runs, so that a function may name a global bound
//! after its `def`; a use that runs before the binding is an error.
//!
//! A function sees its parameters and the names it assigns, then its module's globals and loads,
//! and never its caller's names. At most `MAX_CALLS` functions run at once.
//!
//! Evaluation always ends, on the condition that the crate's doc states: every loop is a
//! comprehension over a list, a tuple or a dict that exists already and never changes, no
//! function runs while it is running, each module runs once, and loads make no cycle. It keeps
//! to its [`Bounds`](crate::Bounds), its values' bytes as `Budget` counts them, and fits in
//! [`STACK_BYTES`](crate::STACK_BYTES): nesting is at most `MAX_NESTING` deep, as the parser
//! counts it, through calls, each running its function's body as deep as its arguments nest. The
//! first error ends it, as Starlark handles none (spec.md, "Module execution"), noting calls.

pub(crate) mod body;
mod call;
mod operators;

use alloc::collections::BTreeMap;
use alloc::collections::btree_map::Entry as Slot;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use crate::builtins::{self, Call, origin};
use crate::eval::body::Host;
use crate::plan::Planned;
use crate::problem::{Fault, Problem, Sources, count, list, refusal};
use crate::pumpkin::Site;
use crate::syntax::{
    self, Args, Clause, Comprehension, Def, Expr, ExprKind, Load, Logic, Module, Name, Operator,
    Simple, Statement, Target, Unary,
};
use crate::value::{Budget, Builtin, Dict, Entry, FileId, Key, Kind, Nested, Span, Text, Value};
use crate::{Label, Loader, Root};

// Two fewer than the 50 frames that starlark-rust's evaluator holds by default: its top level
// takes one, and so does a builtin or a method that the innermost function calls, so that it
// evaluates every file the subset accepts:
// https://github.com/facebook/starlark-rust/blob/85e33edead0189db7a8312005ae8175a30bf155c/starlark/src/eval/runtime/evaluator.rs#L133
const MAX_CALLS: usize = 48;

pub(crate) struct Evaluation<'l, 'h, L> {
    loader: &'l mut L,
    purpose: Purpose<'h>,
    /// Each file read so far.
    pub(crate) sources: Sources,
    pub(crate) planned: Planned<'l>,
    modules: BTreeMap<Label, State>,
    /// The modules that are loading, the first file first, each loaded by the one before it.
    loading: Vec<Loading>,
    /// Each module's top level, by its file.
    tops: Vec<Top>,
    /// The module whose statements run: the first, before any does.
    running: FileId,
    /// The functions running now, outermost first, each with its call.
    calls: Vec<(Rc<Def>, Span)>,
    /// How many expressions it has evaluated, its files' and the functions' they call.
    steps: u64,
    max_steps: u64,
    budget: &'l Budget,
    /// How deep the running function's body starts: the sum of the `depth`s of the calls that
    /// run, each as the parser counts it in its file; 0 at a module's top level.
    depth: usize,
}

enum State {
    /// Its place on `loading`.
    Loading(usize),
    /// Its file, whose [`Top`] its globals are bound in.
    Ran(FileId),
}

// A module that is loading: it runs once each of its loads has.
struct Loading {
    label: Label,
    /// The `load` that loads it; `None` for the first file.
    at: Option<Span>,
    file: FileId,
    module: Module,
    /// How many of the module's loads are bound.
    ran: usize,
}

#[derive(Default)]
struct Top {
    /// `None` until the global is bound.
    globals: BTreeMap<String, Option<Value>>,
    loaded: BTreeMap<String, Value>,
}

/// What an evaluation is for, which decides what its first file is.
pub(crate) enum Purpose<'h> {
    /// A workflow's: its first file is the entry, which alone declares the workflow and images,
    /// and `package` is the entry's directory below the root.
    Workflow { package: String },
    /// A check of a body, which no host calls: its first file is the body's.
    Check,
    /// A host's call of a body: its first file is the body's, and the body's handle reaches
    /// `Host`.
    Call(Host<'h>),
}

impl Purpose<'_> {
    // The entry's directory, if `file` is a workflow's entry.
    fn package(&self, file: FileId) -> Option<&str> {
        match self {
            Self::Workflow { package } => (file == FileId::FIRST).then_some(package.as_str()),
            Self::Check | Self::Call(_) => None,
        }
    }
}

/// `None` until the name is bound.
type Locals<'a> = BTreeMap<&'a str, Option<Value>>;

// Where an expression is evaluated, which decides the names it sees.
#[derive(Clone, Copy)]
enum Scope<'s> {
    /// A module's top level, then its loads.
    Top(FileId),
    /// A function's body: its locals, then its module's top level.
    Function {
        module: FileId,
        locals: &'s Locals<'s>,
    },
    /// A comprehension's variables, then the scope it is in.
    Comprehension {
        names: &'s Locals<'s>,
        outer: &'s Self,
    },
}

impl<'l, 'h, L: Loader> Evaluation<'l, 'h, L> {
    pub(crate) fn new(
        loader: &'l mut L,
        purpose: Purpose<'h>,
        planned: Planned<'l>,
        max_steps: u64,
        budget: &'l Budget,
    ) -> Self {
        Self {
            loader,
            purpose,
            sources: Sources::default(),
            planned,
            modules: BTreeMap::new(),
            loading: Vec::new(),
            tops: Vec::new(),
            running: FileId::FIRST,
            calls: Vec::new(),
            steps: 0,
            max_steps,
            budget,
            depth: 0,
        }
    }

    // Runs the entry file at `label` and, before it, each file it loads.
    pub(crate) fn entry(&mut self, label: &Label) -> Result<(), Vec<Problem>> {
        if label.root() == Root::Library {
            let message = "a file of the standard library can't be an entry, whose directory is \
                           below the task's root: evaluate a `//` file";
            return Err(Vec::from([Problem::file(label, message)]));
        }
        let text = (self.loader.load(label))
            .map_err(|message| Vec::from([Problem::file(label, message)]))?;
        self.first(label, text)
    }

    // Runs `text`, the first file's, at `label`, and, before it, each file it loads. A file's
    // syntax and name errors come all together, any other problem first.
    fn first(&mut self, label: &Label, text: String) -> Result<(), Vec<Problem>> {
        self.open(label, None, text)?;
        // A module waits for its loads on `loading`, not on the call stack, so that a chain of
        // loads may be as long as there are files.
        while let Some(mut top) = self.loading.pop() {
            let Some(load) = top.module.loads.get(top.ran) else {
                self.run(top)?;
                continue;
            };
            match self.modules.get(&load.label) {
                Some(State::Ran(file)) => {
                    self.bind_loaded(load, *file, top.file)?;
                    top.ran += 1;
                    self.loading.push(top);
                }
                Some(State::Loading(first)) => {
                    return Err(Vec::from(self.cycle(*first, &top, load)));
                }
                None => {
                    let (label, at) = (load.label.clone(), load.at);
                    self.loading.push(top);
                    let text = self.loader.load(&label).map_err(|message| {
                        Fault::new(at, format!("loading `{label}`: {message}"))
                    })?;
                    self.open(&label, Some(at), text)?;
                }
            }
        }
        Ok(())
    }

    // The error of `load`, the next of `top`'s, whose module is at `first` on `loading`; `top`,
    // popped off `loading`, ends the chain.
    fn cycle(&self, first: usize, top: &Loading, load: &Load) -> Fault {
        let chain: Vec<&Loading> = self.loading.iter().chain([top]).collect();
        let message = format!(
            "`{}` loads `{}`, which is loading already: break the cycle",
            top.label, load.label
        );
        let mut fault = Fault::new(load.at, message);
        for link in first + 1..chain.len() {
            let (from, to) = (chain[link - 1], chain[link]);
            fault = fault.note(format!("`{}` loads `{}` at", from.label, to.label), to.at);
        }
        fault
    }

    fn open(&mut self, label: &Label, at: Option<Span>, text: String) -> Result<(), Vec<Problem>> {
        let file = self.sources.add(label.clone(), text);
        // `sources` numbers the files in the order they are added, as `tops` holds their top
        // levels.
        self.tops.push(Top::default());
        let module = syntax::parse(file, self.sources.text(file))?;
        let entry = self.purpose.package(file).is_some();
        let mut faults = syntax::resolve(&module, entry);
        if label.root() == Root::Library {
            let task_loads = module
                .loads
                .iter()
                .filter(|load| load.label.root() == Root::Task);
            faults.extend(task_loads.map(|load| {
                let message = refusal(
                    format_args!(
                        "a load of a task's file, `{}`, in a file of the standard library, which \
                         depends on no task,",
                        load.label
                    ),
                    "load only `@pumpkin//` files there",
                );
                Fault::new(load.at, message)
            }));
        }
        if !faults.is_empty() {
            return Err(faults.into_iter().map(Problem::At).collect());
        }
        let place = self.loading.len();
        self.modules.insert(label.clone(), State::Loading(place));
        self.loading.push(Loading {
            label: label.clone(),
            at,
            file,
            module,
            ran: 0,
        });
        Ok(())
    }

    fn bind_loaded(&mut self, load: &Load, from: FileId, into: FileId) -> Result<(), Fault> {
        for name in &load.names {
            let globals = &self.tops[from.0].globals;
            let Some(Some(value)) = globals.get(&name.remote.text) else {
                let loadable: Vec<&str> = (globals.keys())
                    .filter(|global| !global.starts_with('_'))
                    .map(String::as_str)
                    .collect();
                let (label, remote) = (&load.label, &name.remote.text);
                let message = if loadable.is_empty() {
                    format!(
                        "`{label}` binds no global that other files can load: bind `{remote}` in \
                         it"
                    )
                } else {
                    format!(
                        "`{label}` binds no global `{remote}`: load one of {}",
                        list(&loadable)
                    )
                };
                return Err(Fault::new(name.remote.span, message));
            };
            let value = value.clone();
            self.tops[into.0]
                .loaded
                .insert(name.local.text.clone(), value);
        }
        Ok(())
    }

    fn run(&mut self, loading: Loading) -> Result<(), Vec<Problem>> {
        let Loading {
            label,
            at: _,
            file,
            module,
            ran: _,
        } = loading;
        let globals = module.globals().into_iter();
        (self.tops[file.0].globals).extend(globals.map(|name| (name.text.clone(), None)));
        self.running = file;
        let scope = Scope::Top(file);
        for statement in &module.statements {
            match statement {
                Statement::Simple(Simple::Assign { target, value }) => {
                    let value = self.eval(&scope, value)?;
                    let globals = &mut self.tops[file.0].globals;
                    assign(target, value, &mut |name, value| {
                        globals.insert(String::from(name), Some(value));
                    })?;
                }
                Statement::Simple(Simple::Expr(expr)) => drop(self.eval(&scope, expr)?),
                Statement::Simple(Simple::Pass(_)) => {}
                Statement::Def(def) => {
                    let function = self.define(def, file)?;
                    let globals = &mut self.tops[file.0].globals;
                    globals.insert(def.name.text.clone(), Some(function));
                }
            }
        }
        self.modules.insert(label, State::Ran(file));
        Ok(())
    }

    // A comprehension's names, then, in a function, its locals; then the module's globals and
    // loads, and the predeclared names.
    fn lookup(&self, scope: &Scope<'_>, name: &str, span: Span) -> Result<Value, Fault> {
        let mut scope = scope;
        let module = loop {
            match scope {
                Scope::Comprehension { names, outer } => {
                    if let Some(value) = names.get(name) {
                        return bound(value.as_ref(), name, span);
                    }
                    scope = outer;
                }
                Scope::Function { module, locals } => {
                    if let Some(value) = locals.get(name) {
                        return bound(value.as_ref(), name, span);
                    }
                    break *module;
                }
                Scope::Top(module) => break *module,
            }
        };
        let top = &self.tops[module.0];
        if let Some(global) = top.globals.get(name) {
            return bound(global.as_ref(), name, span);
        }
        if let Some(value) = top.loaded.get(name) {
            return Ok(value.clone());
        }
        let Some(kind) = builtins::predeclared(name) else {
            unreachable!(
                "`{name}` resolved to nothing, though resolution refuses a module that uses a \
                 name no block around the use binds, that it neither loads nor has predeclared, \
                 and loads come first, binding every name they load"
            )
        };
        Ok(Value { kind, span })
    }

    fn eval(&mut self, scope: &Scope<'_>, expr: &Expr) -> Result<Value, Fault> {
        let span = expr.span;
        // So that no file runs past `max_steps`, whatever it computes.
        self.steps += 1;
        if self.steps > self.max_steps {
            let message = format!(
                "this evaluation runs past {} steps, an expression's evaluation each: \
                 compute less, as with fewer `for` clauses or smaller lists",
                self.max_steps
            );
            return Err(Fault::new(span, message));
        }
        let kind = match &expr.kind {
            ExprKind::Name(name) => return self.lookup(scope, name, span),
            ExprKind::Int(int) => Kind::Int(*int),
            ExprKind::Float(float) => Kind::Float(*float),
            ExprKind::Str(text) => Kind::Str(Text::literal(text)),
            ExprKind::List(items) => Kind::List(self.elements(scope, items, span)?),
            ExprKind::Tuple(items) => Kind::Tuple(self.elements(scope, items, span)?),
            ExprKind::Dict(entries) => Kind::Dict(self.dict(scope, entries, span)?),
            ExprKind::ListComp {
                body,
                comprehension,
            } => {
                let made = Made::List {
                    body,
                    values: Vec::new(),
                };
                self.comprehension(scope, comprehension, made, span)?
            }
            ExprKind::DictComp {
                key,
                value,
                comprehension,
            } => {
                let made = Made::Dict {
                    key,
                    value,
                    entries: Vec::new(),
                };
                self.comprehension(scope, comprehension, made, span)?
            }
            ExprKind::Call { .. } | ExprKind::Method { .. } | ExprKind::Index { .. } => {
                return self.suffixed(scope, expr);
            }
            ExprKind::Unary {
                operator,
                at,
                operand,
            } => self.unary(scope, *operator, *at, operand)?,
            ExprKind::Logic {
                operator,
                first,
                rest,
            } => return self.logic(scope, *operator, first, rest),
            ExprKind::Binary { first, rest } => return self.binary(scope, first, rest),
            ExprKind::Conditional {
                then,
                condition,
                otherwise,
            } => return self.conditional(scope, then, condition, otherwise),
        };
        Ok(Value { kind, span })
    }

    fn elements(
        &mut self,
        scope: &Scope<'_>,
        items: &[Expr],
        at: Span,
    ) -> Result<Nested<Value>, Fault> {
        let items = (items.iter())
            .map(|item| self.eval(scope, item))
            .collect::<Result<_, _>>()?;
        Nested::new(items, at, self.budget)
    }

    fn dict(
        &mut self,
        scope: &Scope<'_>,
        entries: &[(Expr, Expr)],
        at: Span,
    ) -> Result<Dict, Fault> {
        let mut first: BTreeMap<Key, Span> = BTreeMap::new();
        let mut made = Vec::with_capacity(entries.len());
        for (key_expr, value) in entries {
            let key_value = self.eval(scope, key_expr)?;
            let value = self.eval(scope, value)?;
            let key = key(&key_value, key_expr.span)?;
            match first.entry(key.clone()) {
                Slot::Vacant(slot) => {
                    slot.insert(key_expr.span);
                }
                Slot::Occupied(slot) => {
                    let message = format!(
                        "`{}` is a key of this dict already: give each key once",
                        key.literal()
                    );
                    let fault = Fault::new(key_expr.span, message);
                    return Err(fault.note("it is first given at", Some(*slot.get())));
                }
            }
            made.push(Entry {
                key,
                key_span: key_value.span,
                value,
            });
        }
        Dict::new(made, at, self.budget)
    }

    fn comprehension<'e>(
        &mut self,
        scope: &Scope<'_>,
        comprehension: &'e Comprehension,
        mut made: Made<'e>,
        at: Span,
    ) -> Result<Kind, Fault> {
        let Comprehension {
            target,
            iterable,
            clauses,
        } = comprehension;
        // The `for` clauses' names are one block, unbound until a clause binds them.
        let mut names: Locals<'e> = (comprehension.names().into_iter())
            .map(|name| (name.text.as_str(), None))
            .collect();
        let items = operators::elements(&self.eval(scope, iterable)?, target.span())?;
        // Each `for` that runs, outermost first: what it binds, the clauses after it, and the
        // elements it has yet to bind. They run in a loop, rather than each inside the one before,
        // so that the body, and the calls in it, run under none of the clauses on the stack, as the
        // parser counts none of them around the body.
        let mut fors = Vec::from([(target, clauses.as_slice(), items.into_iter())]);
        while let Some((target, rest, items)) = fors.last_mut() {
            let Some(item) = items.next() else {
                fors.pop();
                continue;
            };
            assign(target, item, &mut |name, value| {
                names.insert(name, Some(value));
            })?;
            let mut rest = *rest;
            loop {
                let scope = Scope::Comprehension {
                    names: &names,
                    outer: scope,
                };
                match rest.split_first() {
                    None => {
                        made.add(self, &scope)?;
                        break;
                    }
                    Some((Clause::If(condition), after)) => {
                        if !self.eval(&scope, condition)?.truth() {
                            break;
                        }
                        rest = after;
                    }
                    Some((Clause::For { target, iterable }, after)) => {
                        let items =
                            operators::elements(&self.eval(&scope, iterable)?, target.span())?;
                        fors.push((target, after, items.into_iter()));
                        break;
                    }
                }
            }
        }
        made.finish(at, self.budget)
    }

    fn arguments<'a>(
        &mut self,
        scope: &Scope<'_>,
        args: &'a Args,
        at: Span,
    ) -> Result<Call<'a>, Fault>
    where
        'l: 'a,
    {
        let positional = (args.positional.iter())
            .map(|arg| Ok((arg.span, self.eval(scope, arg)?)))
            .collect::<Result<_, Fault>>()?;
        let keyword = (args.keyword.iter())
            .map(|(name, arg)| Ok((name, self.eval(scope, arg)?)))
            .collect::<Result<_, Fault>>()?;
        let kwargs = match &args.kwargs {
            Some(kwargs) => Some((kwargs.span, self.eval(scope, kwargs)?)),
            None => None,
        };
        Call::new(at, positional, keyword, kwargs, self.budget)
    }

    fn suffixed(&mut self, scope: &Scope<'_>, expr: &Expr) -> Result<Value, Fault> {
        // They apply in turn, rather than each evaluating the one before it first, so that a
        // suffix's arguments, and the calls in them, run under no more of the chain on the stack
        // than the parser counts around them: the suffixes before it.
        let mut suffixes = Vec::new();
        let mut operand = expr;
        while let Some((suffix, before)) = Suffix::of(operand) {
            suffixes.push(suffix);
            operand = before;
        }
        let mut value = self.eval(scope, operand)?;
        for suffix in suffixes.into_iter().rev() {
            value = match suffix {
                Suffix::Call {
                    callee,
                    args,
                    depth,
                    at,
                } => self.call(scope, value, callee, args, depth, at)?,
                Suffix::Method {
                    receiver,
                    name,
                    args,
                    at,
                } => {
                    let call = self.arguments(scope, args, at)?;
                    builtins::method(&(receiver, value), name, &call, &mut self.purpose)?
                }
                Suffix::Index(index) => {
                    let key = self.eval(scope, index)?;
                    operators::index(&value, &key, index.span)?
                }
            };
        }
        Ok(value)
    }

    fn call(
        &mut self,
        scope: &Scope<'_>,
        function: Value,
        callee: Span,
        args: &Args,
        depth: usize,
        at: Span,
    ) -> Result<Value, Fault> {
        let call = self.arguments(scope, args, at)?;
        match function.kind {
            Kind::Function(function) => self.function(&function, &call, depth),
            Kind::Builtin(Builtin::Universal(universal)) => universal.call(&call),
            Kind::Builtin(Builtin::Pumpkin(pumpkin)) => {
                let site = Site {
                    module: self.sources.label(self.running),
                    package: self.purpose.package(self.running),
                    calls: &self.calls,
                };
                pumpkin.call(&call, &site, &mut self.planned)
            }
            other @ (Kind::None
            | Kind::Bool(_)
            | Kind::Int(_)
            | Kind::Float(_)
            | Kind::Str(_)
            | Kind::List(_)
            | Kind::Tuple(_)
            | Kind::Dict(_)
            | Kind::Image(_)
            | Kind::Module(_)
            | Kind::Hosted(_)) => {
                let message = format!("{} can't be called: call a function", other.describe());
                let fault = Fault::new(callee, message);
                let callee = Value {
                    kind: other,
                    span: function.span,
                };
                Err(origin(fault, "the callee", &callee))
            }
        }
    }

    fn unary(
        &mut self,
        scope: &Scope<'_>,
        operator: Unary,
        at: Span,
        operand: &Expr,
    ) -> Result<Kind, Fault> {
        let operand = self.eval(scope, operand)?;
        match operator {
            Unary::Negate => operators::negate(at, &operand),
            Unary::Not => Ok(Kind::Bool(!operand.truth())),
        }
    }

    fn logic(
        &mut self,
        scope: &Scope<'_>,
        operator: Logic,
        first: &Expr,
        rest: &[Expr],
    ) -> Result<Value, Fault> {
        // The operand that decides is the result, as it is, not as a bool.
        let mut value = self.eval(scope, first)?;
        for operand in rest {
            let decides = match operator {
                Logic::And => !value.truth(),
                Logic::Or => value.truth(),
            };
            if decides {
                return Ok(value);
            }
            value = self.eval(scope, operand)?;
        }
        Ok(value)
    }

    fn binary(
        &mut self,
        scope: &Scope<'_>,
        first: &Expr,
        rest: &[(Operator, Span, Expr)],
    ) -> Result<Value, Fault> {
        let mut left = self.eval(scope, first)?;
        for (operator, at, operand) in rest {
            let right = self.eval(scope, operand)?;
            let kind = operators::binary(*operator, *at, &left, &right, self.budget)?;
            let span = first.span.to(operand.span);
            left = Value { kind, span };
        }
        Ok(left)
    }

    fn conditional(
        &mut self,
        scope: &Scope<'_>,
        then: &Expr,
        condition: &Expr,
        otherwise: &Expr,
    ) -> Result<Value, Fault> {
        if self.eval(scope, condition)?.truth() {
            self.eval(scope, then)
        } else {
            self.eval(scope, otherwise)
        }
    }
}

fn bound(value: Option<&Value>, name: &str, span: Span) -> Result<Value, Fault> {
    value.cloned().ok_or_else(|| {
        let message = format!("`{name}` is used before it is bound: bind it above this use");
        Fault::new(span, message)
    })
}

fn key(value: &Value, at: Span) -> Result<Key, Fault> {
    Key::of(value).map_err(|message| origin(Fault::new(at, message), "the key", value))
}

// Binds each name of `target` to its part of `value`: a target of several names takes a list or a
// tuple of as many.
fn assign<'t>(
    target: &'t Target,
    value: Value,
    bind: &mut impl FnMut(&'t str, Value),
) -> Result<(), Fault> {
    let (targets, span) = match target {
        Target::Name(name) => {
            bind(&name.text, value);
            return Ok(());
        }
        Target::Each { targets, span } => (targets, *span),
    };
    let items = match &value.kind {
        Kind::List(items) | Kind::Tuple(items) => items,
        Kind::Dict(_) => {
            let message = refusal("unpacking a dict", "unpack the list of its `keys()`");
            return Err(origin(Fault::new(span, message), "the value", &value));
        }
        other @ (Kind::None
        | Kind::Bool(_)
        | Kind::Int(_)
        | Kind::Float(_)
        | Kind::Str(_)
        | Kind::Function(_)
        | Kind::Builtin(_)
        | Kind::Image(_)
        | Kind::Module(_)
        | Kind::Hosted(_)) => {
            let message = format!(
                "{} can't be unpacked into names: give a list or a tuple of {}",
                other.describe(),
                targets.len()
            );
            return Err(origin(Fault::new(span, message), "the value", &value));
        }
    };
    if items.len() != targets.len() {
        let message = format!(
            "{} of {} can't be unpacked into {}: give a list or a tuple of {}",
            value.kind.describe(),
            count(items.len(), "value"),
            count(targets.len(), "name"),
            targets.len(),
        );
        return Err(origin(Fault::new(span, message), "the value", &value));
    }
    for (target, item) in targets.iter().zip(items.iter()) {
        assign(target, item.clone(), bind)?;
    }
    Ok(())
}

// One call, method or index of a chain, with the spans that its faults point at.
enum Suffix<'e> {
    Call {
        callee: Span,
        args: &'e Args,
        /// As the parser counts the call's depth.
        depth: usize,
        at: Span,
    },
    Method {
        receiver: Span,
        name: &'e Name,
        args: &'e Args,
        at: Span,
    },
    Index(&'e Expr),
}

impl<'e> Suffix<'e> {
    fn of(expr: &'e Expr) -> Option<(Self, &'e Expr)> {
        let at = expr.span;
        let suffix = match &expr.kind {
            ExprKind::Call {
                callee,
                args,
                depth,
            } => {
                let suffix = Self::Call {
                    callee: callee.span,
                    args,
                    depth: *depth,
                    at,
                };
                (suffix, &**callee)
            }
            ExprKind::Method {
                receiver,
                name,
                args,
            } => {
                let suffix = Self::Method {
                    receiver: receiver.span,
                    name,
                    args,
                    at,
                };
                (suffix, &**receiver)
            }
            ExprKind::Index { object, index } => (Self::Index(index), &**object),
            ExprKind::Name(_)
            | ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::List(_)
            | ExprKind::Tuple(_)
            | ExprKind::Dict(_)
            | ExprKind::ListComp { .. }
            | ExprKind::DictComp { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Logic { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Conditional { .. } => return None,
        };
        Some(suffix)
    }
}

// What a comprehension makes: its body, or its key and value, and what it has made so far.
enum Made<'e> {
    List {
        body: &'e Expr,
        values: Vec<Value>,
    },
    Dict {
        key: &'e Expr,
        value: &'e Expr,
        entries: Vec<Entry>,
    },
}

impl Made<'_> {
    fn add<L: Loader>(
        &mut self,
        evaluation: &mut Evaluation<'_, '_, L>,
        scope: &Scope<'_>,
    ) -> Result<(), Fault> {
        match self {
            Self::List { body, values } => values.push(evaluation.eval(scope, body)?),
            Self::Dict {
                key: key_expr,
                value,
                entries,
            } => {
                let key_value = evaluation.eval(scope, key_expr)?;
                let value = evaluation.eval(scope, value)?;
                entries.push(Entry {
                    key: key(&key_value, key_expr.span)?,
                    key_span: key_value.span,
                    value,
                });
            }
        }
        Ok(())
    }

    // Unlike a dict literal, a comprehension may make a key again: the last value stays.
    fn finish(self, at: Span, budget: &Budget) -> Result<Kind, Fault> {
        match self {
            Self::List { values, .. } => Ok(Kind::List(Nested::new(values, at, budget)?)),
            Self::Dict { entries, .. } => Ok(Kind::Dict(Dict::new(entries, at, budget)?)),
        }
    }
}
