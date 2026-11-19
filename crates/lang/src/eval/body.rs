//! Bodies: a `def` of a loaded file that a host calls during a run, by its file and name, with the
//! arguments that [`crate::call`]'s doc names, on an evaluation of the file of its own, or reads
//! without calling it ([`crate::check`]).
//!
//! A body's file runs as a loaded file does, so a `def` of it gives the same function as in the
//! workflow's evaluation, which is hermetic; what a body must be to be called is
//! [`crate::check`]'s doc's. The host calls the body as starlark-rust's
//! `Evaluator::eval_function` calls a function, with positional and named arguments:
//! <https://github.com/facebook/starlark-rust/blob/85e33edead0189db7a8312005ae8175a30bf155c/starlark/src/eval.rs#L174-L203>.
//!
//! A handle is a value with methods, as a native value of starlark-rust's has them, through a
//! `MethodsBuilder`:
//! <https://github.com/facebook/starlark-rust/blob/85e33edead0189db7a8312005ae8175a30bf155c/starlark/src/values/types/string/methods.rs#L151>.
//! Unlike such a value, a handle holds nothing: its methods reach the host through the
//! evaluation, as a native function of starlark-rust's reaches its embedder's state through
//! `Evaluator::extra_mut`, but typed, where starlark-rust downcasts:
//! <https://github.com/facebook/starlark-rust/blob/85e33edead0189db7a8312005ae8175a30bf155c/starlark/src/eval/runtime/evaluator.rs#L173-L177>.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use schema::ir::{self, AbsolutePath, Argv};

use super::call::refusal_inside;
use super::{Evaluation, Purpose};
use crate::builtins::{Call, Takes, origin, positional, string};
use crate::problem::{Fault, Problem, count, list};
use crate::syntax::{Def, Misfit, Name, Type};
use crate::value::{self, FileId, Function, Hosted, Key, Span, Text, Value};
use crate::{Label, Loader, json};

/// A `def` of a loaded file, which a host calls by its file and name, and the keyword arguments
/// that the grant gives it, the IR's JSON.
pub struct Body<'b> {
    pub file: &'b Label,
    pub def: &'b str,
    pub keywords: &'b serde_json::Map<String, serde_json::Value>,
}

/// The kind of function that a body is, which decides the handle it takes first.
#[derive(Clone, Copy)]
pub enum BodyKind {
    /// A job's body, which drives the call's target through its `Target`.
    Job,
    /// A now function's body, which proposes effects on the run's state through its `Run`.
    Now,
    /// The final call's body, which drives its target through its `Target`, and which no caller
    /// gives an argument after the payload.
    Final,
}

/// The host of a call, which the body's handle reaches: a job's target, the run's state, or the
/// final call's target.
pub enum Host<'h> {
    Job(&'h mut dyn Target),
    Now(&'h mut dyn Run),
    Final(&'h mut dyn Target),
}

impl Host<'_> {
    pub(crate) fn kind(&self) -> BodyKind {
        match self {
            Self::Job(_) => BodyKind::Job,
            Self::Now(_) => BodyKind::Now,
            Self::Final(_) => BodyKind::Final,
        }
    }
}

/// What a job's body drives through its `Target`: the call's target. A method's `Err` is why it
/// failed, which ends the call with that message.
pub trait Target {
    /// Puts the call's payload at `path`, one file, its parents made.
    fn put(&mut self, path: &AbsolutePath) -> Result<(), String>;

    /// Runs `argv`, and waits for it to end, whatever its exit status.
    fn run(&mut self, argv: &Argv) -> Result<(), String>;

    /// The text of the file at `path`, all of it: the call charges it to the bytes of its
    /// [`Bounds`](crate::Bounds) once it is returned, so a text longer than they have left ends
    /// the call.
    fn get(&mut self, path: &AbsolutePath) -> Result<String, String>;
}

/// What a now function's body proposes effects on through its `Run`: the run's state.
pub trait Run {
    /// Proposes that the call's payload become the run's selection, replacing the last.
    fn select(&mut self);

    /// Proposes that the caller's work end, its selection locked.
    fn end_work(&mut self);
}

/// A body read against its kind: the kind, its function, and the grant's keywords, each a value
/// that fits the keyword-only parameter it names.
pub(crate) struct Read {
    kind: BodyKind,
    function: Rc<Function>,
    keywords: Vec<(Name, Value)>,
}

impl BodyKind {
    // How a message names a body of this kind.
    fn words(self) -> &'static str {
        match self {
            Self::Job => "a job's body",
            Self::Now => "a now function's body",
            Self::Final => "the final call's body",
        }
    }

    // What gives a body of this kind its keywords, as a message names it.
    fn keywords_from(self) -> &'static str {
        match self {
            Self::Job | Self::Now => "the grant's",
            Self::Final => "the final call's",
        }
    }

    // The refusal of `def`'s keyword-only parameter `keyword`, which has no default and which the
    // call doesn't give.
    fn ungiven(self, def: &str, keyword: &str) -> String {
        match self {
            Self::Job | Self::Final => format!(
                "`{def}` takes the keyword `{keyword}`, which {} `keywords` does not give: give \
                 it, or a default",
                self.keywords_from()
            ),
            Self::Now => format!(
                "`{def}` takes the keyword `{keyword}`, and a now function has no `keywords` to \
                 give it: give it a default, or make it a positional parameter after the \
                 payload, for its caller to give"
            ),
        }
    }

    // The handle that a body of this kind takes first, how an annotation writes its type, and the
    // name that a message gives its parameter.
    fn handle(self) -> (Hosted, &'static str, &'static str) {
        match self {
            Self::Job | Self::Final => (Hosted::Target, "Target", "target"),
            Self::Now => (Hosted::Run, "Run", "run"),
        }
    }
}

impl<L: Loader> Evaluation<'_, '_, L> {
    /// The body that `body` names, once its file and the files it loads have run, read against
    /// `kind`, as [`crate::check`] has it.
    pub(crate) fn body(&mut self, body: &Body<'_>, kind: BodyKind) -> Result<Read, Vec<Problem>> {
        let text = (self.loader.load(body.file))
            .map_err(|message| Vec::from([Problem::file(body.file, message)]))?;
        self.first(body.file, text)?;
        let function = self.defined(body)?;
        let keywords = self.keywords(&function, body.keywords, kind);
        let mut faults = parameters(&function, kind);
        match keywords {
            Ok(keywords) if faults.is_empty() => {
                return Ok(Read {
                    kind,
                    function,
                    keywords,
                });
            }
            Ok(_) => {}
            Err(more) => faults.extend(more),
        }
        faults.sort_by_key(|fault| fault.at().start);
        Err(faults.into_iter().map(Problem::At).collect())
    }

    /// What the body of `read` returns, called as [`crate::call`] calls it.
    pub(crate) fn returned(
        &mut self,
        read: &Read,
        arguments: &BTreeMap<ir::Name, ir::ArgumentValue>,
    ) -> Result<Vec<(ir::Name, serde_json::Value)>, Fault> {
        let (handle, _, _) = read.kind.handle();
        let at = read.function.def.name.span;
        let positional = Vec::from([handle, Hosted::Payload]);
        let positional = (positional.into_iter())
            .map(|hosted| {
                let kind = value::Kind::Hosted(hosted);
                (at, Value { kind, span: at })
            })
            .collect();
        let mut keywords: Vec<(&Name, Value)> = (read.keywords.iter())
            .map(|(name, value)| (name, value.clone()))
            .collect();
        keywords.extend(caller_arguments(
            &read.function.def,
            arguments,
            at,
            self.budget,
        )?);
        let call = Call::of(at, positional, keywords, self.budget);
        let returned = self.function(&read.function, &call, 0)?;
        let def = &read.function.def.name.text;
        let value::Kind::Dict(dict) = &returned.kind else {
            let message = format!(
                "`{def}` returns {}, and a body returns a dict of names to values: return a dict",
                returned.shown()
            );
            return Err(origin(
                Fault::new(returned.span, message),
                "the value",
                &returned,
            ));
        };
        let mut values = Vec::with_capacity(dict.len());
        for entry in dict.iter() {
            let named = match &entry.key {
                Key::Str(key) => ir::Name::try_from(String::from(&**key)),
                Key::Number(_) => Err(format!(
                    "`{}` is no string: key it with a name",
                    entry.key.literal()
                )),
            };
            let name = named.map_err(|problem| {
                let message =
                    format!("a key of the dict that `{def}` returns is no name: {problem}");
                origin(
                    Fault::new(returned.span, message),
                    "the key",
                    &entry.key_value(),
                )
            })?;
            let text = json::encode(&(returned.span, entry.value.clone()), self.budget).map_err(
                |fault| {
                    let note = format!("it is `{name}` of what `{def}` returns, as JSON");
                    fault.note(note, None)
                },
            )?;
            let Ok(json) = serde_json::from_str(&text) else {
                unreachable!(
                    "`serde_json` refuses the text that `json.encode` writes of a dict's value, \
                     though that text is JSON nested no deeper than `serde_json` reads, as a \
                     value nests at most `MAX_NESTING` deep, whose doc says how the two limits \
                     stand, and the dict takes one of those levels"
                )
            };
            values.push((name, json));
        }
        Ok(values)
    }

    // The function that a `def` of the first file, which has run, makes under `body.def`.
    fn defined(&self, body: &Body<'_>) -> Result<Rc<Function>, Vec<Problem>> {
        let globals = &self.tops[FileId::FIRST.0].globals;
        // A global is bound once, so the one that a `def` binds holds its function.
        let made = |value: &Value| match &value.kind {
            value::Kind::Function(function) => {
                (function.module == FileId::FIRST).then(|| Rc::clone(function))
            }
            value::Kind::None
            | value::Kind::Bool(_)
            | value::Kind::Int(_)
            | value::Kind::Float(_)
            | value::Kind::Str(_)
            | value::Kind::List(_)
            | value::Kind::Tuple(_)
            | value::Kind::Dict(_)
            | value::Kind::Builtin(_)
            | value::Kind::Image(_)
            | value::Kind::Module(_)
            | value::Kind::Hosted(_) => None,
        };
        let found = globals
            .get(body.def)
            .and_then(Option::as_ref)
            .and_then(made);
        if let Some(function) = found.filter(|function| function.def.name.text == body.def) {
            return Ok(function);
        }
        let defined: Vec<&str> = (globals.iter())
            .filter_map(|(name, value)| {
                let function = made(value.as_ref()?)?;
                (function.def.name.text == *name).then_some(name.as_str())
            })
            .collect();
        let def = body.def.escape_debug();
        let message = if defined.is_empty() {
            format!("the file defines no function, so not `{def}`: name the file that defines it")
        } else {
            format!(
                "the file defines no function `{def}`: name one of {}",
                list(&defined)
            )
        };
        Err(Vec::from([Problem::file(body.file, message)]))
    }

    // `given`, the keyword arguments that the call of `function` passes, each a value, or the
    // refusals that `crate::check`'s doc names.
    fn keywords(
        &self,
        function: &Function,
        given: &serde_json::Map<String, serde_json::Value>,
        kind: BodyKind,
    ) -> Result<Vec<(Name, Value)>, Vec<Fault>> {
        let def = &*function.def;
        let name = &def.name.text;
        let at = def.name.span;
        let from = kind.keywords_from();
        let mut faults = Vec::new();
        let keyword_only = def
            .params
            .iter()
            .zip(&function.defaults)
            .skip(def.positional);
        for (param, default) in keyword_only.clone() {
            if default.is_none() && !given.contains_key(&param.name.text) {
                let message = kind.ungiven(name, &param.name.text);
                faults.push(Fault::new(param.name.span, message));
            }
        }
        let mut keywords = Vec::new();
        for (keyword, json) in given {
            let Some((param, _)) = (keyword_only.clone()).find(|(p, _)| p.name.text == *keyword)
            else {
                let message = format!(
                    "{from} `keywords` gives `{}`, which is no keyword-only parameter of \
                     `{name}`: remove it",
                    keyword.escape_debug()
                );
                faults.push(Fault::new(at, message));
                continue;
            };
            let value = match json::value_of(json, at, self.budget) {
                Ok(value) => value,
                Err(fault) => {
                    faults.push(fault.note(format!("it is {from} keyword `{keyword}`"), None));
                    continue;
                }
            };
            if let Some(annotation) = &param.annotation {
                match annotation.misfit(&value) {
                    None => {}
                    Some(Misfit::Whole) => {
                        let message = format!(
                            "{from} keyword `{keyword}` can't be {}: `{name}` takes {}",
                            value.shown(),
                            annotation.describe()
                        );
                        faults.push(Fault::new(param.name.span, message));
                        continue;
                    }
                    Some(Misfit::Inside(inside)) => {
                        let whole = format!("{from} keyword `{keyword}`");
                        faults.push(refusal_inside(&inside, &whole, param.name.span));
                        continue;
                    }
                }
            }
            let name = Name {
                text: keyword.clone(),
                span: at,
            };
            keywords.push((name, value));
        }
        if faults.is_empty() {
            Ok(keywords)
        } else {
            Err(faults)
        }
    }
}

// `arguments`, each the value of the positional parameter after the payload of `def`'s that it
// names, as a keyword argument made at `at`.
fn caller_arguments<'d>(
    def: &'d Def,
    arguments: &BTreeMap<ir::Name, ir::ArgumentValue>,
    at: Span,
    budget: &value::Budget,
) -> Result<Vec<(&'d Name, Value)>, Fault> {
    let caller = def.params.iter().take(def.positional).skip(2);
    let mut bound = Vec::with_capacity(arguments.len());
    for (name, argument) in arguments {
        let Some(param) = caller
            .clone()
            .find(|param| *param.name.text == *name.as_str())
        else {
            let message = format!(
                "`{}` takes no `{name}` after the payload, which its caller gives: give only \
                 those it takes",
                def.name.text
            );
            return Err(Fault::new(at, message));
        };
        let kind = match argument {
            ir::ArgumentValue::Str(text) => value::Kind::Str(Text::new(text, at, budget)?),
            ir::ArgumentValue::Int(int) => value::Kind::Int(*int),
            ir::ArgumentValue::Bool(bool) => value::Kind::Bool(*bool),
        };
        bound.push((&param.name, Value { kind, span: at }));
    }
    Ok(bound)
}

// The refusals of `function`'s positional parameters, against a call of `kind`, that
// `crate::check`'s doc names.
fn parameters(function: &Function, kind: BodyKind) -> Vec<Fault> {
    let def = &*function.def;
    let name = &def.name.text;
    let (handle, written, handle_name) = kind.handle();
    let words = kind.words();
    let positional = &def.params[..def.positional];
    let [first, second, caller @ ..] = positional else {
        let message = format!(
            "`{name}` takes {}, and {words} takes its `{written}` and the payload first: write \
             `def {name}({handle_name}: {written}, payload: Payload, …)`",
            count(positional.len(), "positional parameter"),
        );
        return Vec::from([Fault::new(def.name.span, message)]);
    };
    let mut faults = Vec::new();
    for param in [first, second] {
        if let Some(default) = &param.default {
            let message = format!(
                "`{name}`'s `{}` has a default, and {words} is always given its `{written}` and \
                 the payload: drop the default",
                param.name.text
            );
            faults.push(Fault::new(default.span, message));
        }
    }
    if let Some(annotation) = &first.annotation
        && *annotation != Type::Hosted(handle)
    {
        let message = format!(
            "`{name}`'s first parameter is {}, and {words} takes its `{written}`: write `def \
             {name}({}: {written}, …)`",
            annotation.describe(),
            first.name.text,
        );
        faults.push(Fault::new(first.name.span, message));
    }
    if let Some(annotation) = &second.annotation
        && *annotation != Type::Hosted(Hosted::Payload)
    {
        let message = format!(
            "`{name}`'s second parameter is {}, and {words} takes the payload second: annotate \
             `{}` `Payload`",
            annotation.describe(),
            second.name.text
        );
        faults.push(Fault::new(second.name.span, message));
    }
    for param in caller {
        if let BodyKind::Final = kind {
            let message = format!(
                "`{name}`'s `{}` follows the payload, and the final call has no caller to give \
                 it: remove it",
                param.name.text
            );
            faults.push(Fault::new(param.name.span, message));
        } else if !matches!(param.annotation, Some(Type::Str | Type::Int | Type::Bool)) {
            let message = format!(
                "`{name}`'s `{}` follows the payload, so its caller gives it: annotate it `str`, \
                 `int` or `bool`",
                param.name.text
            );
            faults.push(Fault::new(param.name.span, message));
        }
    }
    faults
}

#[derive(Clone, Copy)]
pub(crate) enum Method {
    Target(fn(&mut dyn Target, &Name, &Call<'_>) -> Result<value::Kind, Fault>),
    Run(fn(&mut dyn Run, &Name, &Call<'_>) -> Result<value::Kind, Fault>),
}

impl Hosted {
    pub(crate) fn methods(self) -> Result<&'static [(&'static str, Method)], &'static str> {
        match self {
            Self::Target => Ok(&[
                ("get", Method::Target(get)),
                ("put", Method::Target(put)),
                ("run", Method::Target(run)),
            ]),
            Self::Run => Ok(&[
                ("end_work", Method::Run(end_work)),
                ("select", Method::Run(select)),
            ]),
            Self::Payload => Err("pass it to a `Target`'s `put` or a `Run`'s `select`"),
        }
    }
}

impl Method {
    pub(crate) fn call(
        self,
        purpose: &mut Purpose<'_>,
        name: &Name,
        call: &Call<'_>,
    ) -> Result<Value, Fault> {
        let kind = match self {
            Self::Target(method) => {
                let Purpose::Call(Host::Job(target) | Host::Final(target)) = purpose else {
                    unreachable!(
                        "a `Target`'s method is called outside a call of a job's or the final \
                         call's body, though only such a call makes a `Target`, and the value it \
                         is in lives no longer than its evaluation"
                    )
                };
                method(&mut **target, name, call)
            }
            Self::Run(method) => {
                let Purpose::Call(Host::Now(run)) = purpose else {
                    unreachable!(
                        "a `Run`'s method is called outside a call of a now function's body, \
                         though only that call makes a `Run`, and the value it is in lives no \
                         longer than its evaluation"
                    )
                };
                method(&mut **run, name, call)
            }
        }?;
        Ok(Value {
            kind,
            span: call.at,
        })
    }
}

fn put(target: &mut dyn Target, name: &Name, call: &Call<'_>) -> Result<value::Kind, Fault> {
    let [path, payload] = positional::<2>(&name.text, Takes::exactly(2), call)?;
    let path = absolute("`put`'s path", path)?;
    if !matches!(payload.1.kind, value::Kind::Hosted(Hosted::Payload)) {
        let message = format!(
            "`put` can't put {}: give it the payload",
            payload.1.kind.describe()
        );
        return Err(origin(
            Fault::new(payload.0, message),
            "the argument",
            &payload.1,
        ));
    }
    target
        .put(&path)
        .map_err(|message| Fault::new(call.at, message))?;
    Ok(value::Kind::None)
}

fn run(target: &mut dyn Target, name: &Name, call: &Call<'_>) -> Result<value::Kind, Fault> {
    let [argv] = positional::<1>(&name.text, Takes::exactly(1), call)?;
    let refused = |what: &Value| {
        let message = format!(
            "`run` can't run {}: give it a list of strings",
            what.kind.describe()
        );
        origin(Fault::new(argv.0, message), "the argument", what)
    };
    let value::Kind::List(items) = &argv.1.kind else {
        return Err(refused(&argv.1));
    };
    let mut words = Vec::with_capacity(items.len());
    for item in items.iter() {
        let value::Kind::Str(text) = &item.kind else {
            return Err(refused(item));
        };
        words.push(String::from(&**text));
    }
    let argv = Argv::try_from(words)
        .map_err(|problem| origin(Fault::new(argv.0, problem), "the argument", &argv.1))?;
    target
        .run(&argv)
        .map_err(|message| Fault::new(call.at, message))?;
    Ok(value::Kind::None)
}

fn get(target: &mut dyn Target, name: &Name, call: &Call<'_>) -> Result<value::Kind, Fault> {
    let [path] = positional::<1>(&name.text, Takes::exactly(1), call)?;
    let path = absolute("`get`'s path", path)?;
    let text = target
        .get(&path)
        .map_err(|message| Fault::new(call.at, message))?;
    Ok(value::Kind::Str(Text::new(&text, call.at, call.budget)?))
}

fn select(run: &mut dyn Run, name: &Name, call: &Call<'_>) -> Result<value::Kind, Fault> {
    let [payload] = positional::<1>(&name.text, Takes::exactly(1), call)?;
    if !matches!(payload.1.kind, value::Kind::Hosted(Hosted::Payload)) {
        let message = format!(
            "`select` can't select {}: give it the payload",
            payload.1.kind.describe()
        );
        return Err(origin(
            Fault::new(payload.0, message),
            "the argument",
            &payload.1,
        ));
    }
    run.select();
    Ok(value::Kind::None)
}

fn end_work(run: &mut dyn Run, name: &Name, call: &Call<'_>) -> Result<value::Kind, Fault> {
    positional::<0>(&name.text, Takes::exactly(0), call)?;
    run.end_work();
    Ok(value::Kind::None)
}

// The absolute path that `argument`, `what`, gives.
fn absolute(what: &str, argument: &(Span, Value)) -> Result<AbsolutePath, Fault> {
    let path = string(what, argument.0, &argument.1)?;
    AbsolutePath::try_from(String::from(path))
        .map_err(|problem| origin(Fault::new(argument.0, problem), "the argument", &argument.1))
}
