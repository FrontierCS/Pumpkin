//! Pumpkin's predeclared names: the functions that make an evaluation's workflow and images and
//! read its `--arg`s ([`Args`](crate::Args)); the units `KiB`, `MiB` and `GiB`, the ints 2^10,
//! 2^20 and 2^30, and `USD`, the int 10^6, a US dollar in the millionths that a model budget
//! counts (`schema::ir::ModelBudget`); and two values of the contract between a sandbox and what
//! runs in it, which `schema::sandbox` holds: `MODEL_URL`, the string of the model endpoint's
//! URL, its `model_url()`, and `MAX_MODEL_REQUESTS`, the int of its
//! `MAX_MODEL_REQUESTS_IN_FLIGHT`.
//!
//! Each function takes its arguments as a `def` of its signature would. All but `arg()` refuse a
//! call made while a file other than the entry loads ([`Site::in_entry`]). Each records what it
//! declares in [`Planned`].

use alloc::collections::BTreeMap;
use alloc::collections::btree_map::Entry as Slot;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use schema::sandbox::{MAX_MODEL_REQUESTS_IN_FLIGHT, RelativePath, model_url};

use crate::builtins::{Call, Universal, called, lacks, place, string};
use crate::plan::{Image, Place, Planned, Recipe, WorkflowCall, declare};
use crate::problem::Fault;
use crate::syntax::{Def, is_name};
use crate::value::{
    Budget, Builtin, Dict, Entry, ImageId, Key, Kind, Span, Text, Value, escape, quote,
};
use crate::{Label, Root, is_component};

// `MAX_MODEL_REQUESTS_IN_FLIGHT`, as an int of the language.
#[expect(
    clippy::cast_possible_wrap,
    reason = "the assertion below holds the cap within an int"
)]
const MAX_MODEL_REQUESTS: i64 = MAX_MODEL_REQUESTS_IN_FLIGHT as i64;

// A cap past 2^63 - 1 would wrap to a negative int.
const _: () = assert!(MAX_MODEL_REQUESTS > 0);

pub(crate) fn predeclared(name: &str) -> Option<Kind> {
    let kind = match name {
        "KiB" => Kind::Int(1 << 10),
        "MiB" => Kind::Int(1 << 20),
        "GiB" => Kind::Int(1 << 30),
        "USD" => Kind::Int(1_000_000),
        "MODEL_URL" => Kind::Str(Text::literal(&Rc::from(model_url()))),
        "MAX_MODEL_REQUESTS" => Kind::Int(MAX_MODEL_REQUESTS),
        _ => {
            let pumpkin = Pumpkin::ALL
                .into_iter()
                .find(|pumpkin| pumpkin.name() == name)?;
            Kind::Builtin(Builtin::Pumpkin(pumpkin))
        }
    };
    Some(kind)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pumpkin {
    Workflow,
    Arg,
    Image,
    PackageName,
}

impl Pumpkin {
    const ALL: [Self; 4] = [Self::Workflow, Self::Arg, Self::Image, Self::PackageName];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Workflow => "workflow",
            Self::Arg => "arg",
            Self::Image => "image",
            Self::PackageName => "package_name",
        }
    }

    pub(crate) fn call(
        self,
        call: &Call<'_>,
        site: &Site<'_>,
        planned: &mut Planned<'_>,
    ) -> Result<Value, Fault> {
        match self {
            Self::Workflow => workflow(call, site, planned),
            Self::Arg => arg(call, planned),
            Self::Image => image(call, site, planned),
            Self::PackageName => package_name(call, site),
        }
    }
}

// Where a function of Pumpkin's is called.
pub(crate) struct Site<'e> {
    /// The file whose statements are running.
    pub(crate) module: &'e Label,
    /// The entry's directory below the root, while the statements running are a workflow's
    /// entry's; else `None`.
    pub(crate) package: Option<&'e str>,
    /// The functions running, outermost first, each with its call.
    pub(crate) calls: &'e [(Rc<Def>, Span)],
}

impl<'e> Site<'e> {
    // The entry's directory, `package`, refused unless the entry's own statements are running. A
    // file may not depend on what loads it (spec.md, "Module execution"), so what a task
    // declares, the entry file's own statements declare, and loading a file declares nothing.
    fn in_entry(&self, function: Pumpkin, at: Span) -> Result<&'e str, Fault> {
        if let Some(package) = self.package {
            return Ok(package);
        }
        let (function, module) = (function.name(), self.module);
        let message = format!(
            "`{function}()` is called while `{module}` loads: call it in the entry file, which \
             loads `{module}`"
        );
        Err(Fault::new(at, message))
    }

    fn place(&self, at: Span) -> Place {
        let calls = self.calls.iter().rev();
        Place {
            at,
            calls: calls.map(|(def, at)| (called(def), *at)).collect(),
        }
    }
}

// The parameters of one of Pumpkin's functions, by name: `P` that a call may give by position or
// by keyword, then `R` and `O` that it gives by keyword only; each of the `P` and the `R` is
// required.
struct Params<const P: usize, const R: usize, const O: usize> {
    positional: [&'static str; P],
    required: [&'static str; R],
    optional: [&'static str; O],
}

// The arguments a call gives [`Params`], in their order.
struct Bound<const P: usize, const R: usize, const O: usize> {
    positional: [Value; P],
    required: [Value; R],
    optional: [Option<Value>; O],
}

impl<const P: usize, const R: usize, const O: usize> Params<P, R, O> {
    // The arguments that `call` gives `function`, whose parameters these are; refused as [`place`]
    // refuses, or if a required one is missing.
    fn bind(&self, function: Pumpkin, call: &Call<'_>) -> Result<Bound<P, R, O>, Fault> {
        let name = function.name();
        let params: Vec<&str> = (self.positional.iter())
            .chain(&self.required)
            .chain(&self.optional)
            .copied()
            .collect();
        let mut given = place(name, &params, P, call)?.into_iter();
        let mut missing = Vec::new();
        // A missing argument stands as `None` until the refusal below, which names every one.
        let mut take = |param: &'static str| {
            given.next().flatten().unwrap_or_else(|| {
                missing.push(param);
                Value {
                    kind: Kind::None,
                    span: call.at,
                }
            })
        };
        let positional = self.positional.map(&mut take);
        let required = self.required.map(&mut take);
        let optional = self.optional.map(|_| given.next().flatten());
        if !missing.is_empty() {
            return Err(lacks(name, &missing, call.at));
        }
        Ok(Bound {
            positional,
            required,
            optional,
        })
    }
}

// `workflow(*, name, roles, score)`: the evaluation's workflow is the one whose IR has these
// fields, at the version this crate reads. It returns `None`.
fn workflow(call: &Call<'_>, site: &Site<'_>, planned: &mut Planned<'_>) -> Result<Value, Fault> {
    const PARAMS: Params<0, 3, 0> = Params {
        positional: [],
        required: ["name", "roles", "score"],
        optional: [],
    };
    // Bound only to check the call: the dict keeps the arguments in the order given, each key at
    // its keyword.
    PARAMS.bind(Pumpkin::Workflow, call)?;
    let at = call.at;
    site.in_entry(Pumpkin::Workflow, at)?;
    if let Some(first) = &planned.workflow {
        let message = "`workflow()` is called a second time: call it once";
        return Err(Fault::new(at, message).note("it is first called at", Some(first.at)));
    }
    let fields = (call.keywords())
        .map(|keyword| {
            Ok(Entry {
                key: Key::Str(Text::new(keyword.name, keyword.span, call.budget)?),
                key_span: keyword.span,
                value: keyword.value.clone(),
            })
        })
        .collect::<Result<_, Fault>>()?;
    let fields = Value {
        kind: Kind::Dict(Dict::new(fields, at, call.budget)?),
        span: at,
    };
    planned.workflow = Some(WorkflowCall { at, fields });
    Ok(Value {
        kind: Kind::None,
        span: at,
    })
}

// `arg(name, type, *, default)`: the value of `--arg <name>=<value>`, read as `type`, which is
// `str`, `int` or `bool`. A call with no `default` requires the arg, and `None` is a default for
// any type.
fn arg(call: &Call<'_>, planned: &mut Planned<'_>) -> Result<Value, Fault> {
    const PARAMS: Params<2, 0, 1> = Params {
        positional: ["name", "type"],
        required: [],
        optional: ["default"],
    };
    let Bound {
        positional: [name, kind],
        required: [],
        optional: [default],
    } = PARAMS.bind(Pumpkin::Arg, call)?;
    let at = call.at;
    let text = string("`arg`'s `name`", name.span, &name)?;
    if !is_name(text) {
        let message = format!(
            "`{}` is no arg's name: name it as Starlark names a value, with ASCII letters, \
             digits and `_`, not starting with a digit",
            escape(text)
        );
        return Err(Fault::new(name.span, message));
    }
    let kind = Type::of(&kind)?;
    let default = default.map(|default| kind.default(default)).transpose()?;
    match planned.declared.entry(String::from(text)) {
        Slot::Vacant(slot) => {
            slot.insert(at);
        }
        Slot::Occupied(first) => {
            let message = format!("`--arg {text}` is declared already: declare each arg once");
            let fault = Fault::new(at, message);
            return Err(fault.note("it is first declared at", Some(*first.get())));
        }
    }
    let value = match (planned.args.0.get(text), default) {
        (Some(given), _) => kind.read(text, given, at, call.budget),
        (None, Some(default)) => Ok(default),
        (None, None) => {
            let message = format!(
                "`--arg {text}` is required, as this `arg()` gives it no default: give it as \
                 `--arg {text}=<value>`"
            );
            Err(Fault::new(at, message))
        }
    };
    // The refusal ends the evaluation before each `--arg` that no `arg()` declares is refused, and
    // one of them may be this arg's name misspelled.
    value.map_err(|fault| {
        (planned.given_undeclared()).fold(fault, |fault, name| {
            let name = name.escape_debug();
            let note =
                format!("`--arg {name}` is given, and no `arg()` call before this one declares it");
            fault.note(note, None)
        })
    })
}

#[derive(Clone, Copy)]
enum Type {
    Str,
    Int,
    Bool,
}

impl Type {
    fn of(value: &Value) -> Result<Self, Fault> {
        let kind = match &value.kind {
            Kind::Builtin(Builtin::Universal(universal)) => match universal {
                Universal::Str => Some(Self::Str),
                Universal::Int => Some(Self::Int),
                Universal::Bool => Some(Self::Bool),
                Universal::Float => {
                    let message = "an arg can't be a float: give `arg`'s `type` as the builtin \
                                   `str`, `int` or `bool`";
                    return Err(Fault::new(value.span, String::from(message)));
                }
                Universal::Fail | Universal::Len | Universal::Sorted => None,
            },
            Kind::None
            | Kind::Bool(_)
            | Kind::Int(_)
            | Kind::Float(_)
            | Kind::Str(_)
            | Kind::List(_)
            | Kind::Tuple(_)
            | Kind::Dict(_)
            | Kind::Function(_)
            | Kind::Builtin(Builtin::Pumpkin(_))
            | Kind::Image(_)
            | Kind::Module(_)
            | Kind::Hosted(_) => None,
        };
        kind.ok_or_else(|| {
            let message = format!(
                "`arg`'s `type` can't be {}: give the builtin `str`, `int` or `bool`",
                value.shown()
            );
            Fault::new(value.span, message)
        })
    }

    fn words(self) -> (&'static str, &'static str) {
        match self {
            Self::Str => ("str", "a string"),
            Self::Int => ("int", "an int"),
            Self::Bool => ("bool", "a bool"),
        }
    }

    fn default(self, default: Value) -> Result<Value, Fault> {
        // `None` leaves the program to pick the value of an arg not given, from what it knows, such
        // as a task's own values.
        let fits = matches!(
            (self, &default.kind),
            (_, Kind::None)
                | (Self::Str, Kind::Str(_))
                | (Self::Int, Kind::Int(_))
                | (Self::Bool, Kind::Bool(_))
        );
        if fits {
            return Ok(default);
        }
        let (builtin, value) = self.words();
        let message = format!(
            "an arg of type `{builtin}` can't default to {}: give {value} or `None`",
            default.shown()
        );
        Err(Fault::new(default.span, message))
    }

    fn read(self, name: &str, text: &str, at: Span, budget: &Budget) -> Result<Value, Fault> {
        let shown = quote(text);
        let kind = match self {
            Self::Str => Kind::Str(Text::new(text, at, budget)?),
            Self::Int => {
                let Ok(int) = text.parse::<i64>() else {
                    let message = format!(
                        "`--arg {name}` is `{shown}`, which `int` can't read: give it decimal \
                         digits, after `-` or `+` or neither, from -2^63 to 2^63 - 1"
                    );
                    return Err(Fault::new(at, message));
                };
                Kind::Int(int)
            }
            Self::Bool => match text {
                "true" => Kind::Bool(true),
                "false" => Kind::Bool(false),
                _ => {
                    let message = format!(
                        "`--arg {name}` is `{shown}`, which is no bool: give it `true` or `false`"
                    );
                    return Err(Fault::new(at, message));
                }
            },
        };
        Ok(Value { kind, span: at })
    }
}

// `image(recipe, *, context = ".", mount = {}, target = None, args = {}, contexts = {})`: the
// image that `recipe`, a file of `context`, builds, with each directory of `mount` at its path in
// the context, and its stage `target`, in any case, given each string of `args` and image of
// `contexts` as the build argument and the named context of its key. The context may not be the
// root.
fn image(call: &Call<'_>, site: &Site<'_>, planned: &mut Planned<'_>) -> Result<Value, Fault> {
    const PARAMS: Params<1, 0, 5> = Params {
        positional: ["recipe"],
        required: [],
        optional: ["context", "mount", "target", "args", "contexts"],
    };
    let Bound {
        positional: [recipe],
        required: [],
        optional: [context, mount, target, args, contexts],
    } = PARAMS.bind(Pumpkin::Image, call)?;
    let at = call.at;
    let package = site.in_entry(Pumpkin::Image, at)?;
    let file = string("`image`'s `recipe`", recipe.span, &recipe)?;
    let file = in_context(file, recipe.span)?;
    let (context, context_at) = match context {
        Some(context) => {
            let text = string("`image`'s `context`", context.span, &context)?;
            (directory(package, text, context.span)?, context.span)
        }
        None if package.is_empty() => {
            let message = "the context defaults to the entry file's directory, the root, which \
                           can't be a context: give `context` a directory below it";
            return Err(Fault::new(at, message));
        }
        None => (directory(package, ".", at)?, at),
    };
    let mounts = mounts(package, mount.as_ref())?;
    let target = stage(target)?;
    let args = build_args(args.as_ref())?;
    let contexts = named(contexts.as_ref())?;
    names_repository(context.path(), context_at)?;
    let repositories = match context.root() {
        Root::Task => REPOSITORIES,
        Root::Library => LIBRARY_REPOSITORIES,
    };
    let repository = match file.as_str().rsplit_once('/') {
        Some((dir, _)) => {
            names_repository(dir, recipe.span)?;
            format!("{repositories}/{}/{dir}", context.path())
        }
        None => format!("{repositories}/{}", context.path()),
    };
    let repository = fits(repository, at)?;
    let recipe = Recipe {
        context,
        recipe: file,
        mounts,
        target,
        args,
        repository,
    };
    let image = Image { recipe, contexts };
    let image = declare(&mut planned.images, image, site.place(at));
    Ok(Value {
        kind: Kind::Image(image),
        span: at,
    })
}

fn keyed<'v>(
    what: &'static str,
    dict: Option<&'v Value>,
    values: &'static str,
    key: &'static str,
) -> Result<impl Iterator<Item = Result<(&'v str, &'v Entry), Fault>>, Fault> {
    let entries: &[Entry] = match dict {
        None => &[],
        Some(Value {
            kind: Kind::Dict(entries),
            ..
        }) => entries,
        Some(other) => {
            let message = format!(
                "{what} can't be {}: give a dict of {values}, each by {key}",
                other.shown()
            );
            return Err(Fault::new(other.span, message));
        }
    };
    Ok(entries.iter().map(move |entry| match &entry.key {
        Key::Str(text) => Ok((&**text, entry)),
        Key::Number(_) => {
            let message = format!(
                "{what} can't key a value by `{}`: key it by {key}, a string",
                entry.key.literal()
            );
            Err(Fault::new(entry.key_span, message))
        }
    }))
}

fn mounts(package: &str, mount: Option<&Value>) -> Result<BTreeMap<RelativePath, Label>, Fault> {
    let entries = keyed(
        "`image`'s `mount`",
        mount,
        "directories",
        "its path in the context",
    )?;
    entries
        .map(|entry| {
            let (key, entry) = entry?;
            let at = in_context(key, entry.key_span)?;
            let value = &entry.value;
            let text = string("a directory of `image`'s `mount`", value.span, value)?;
            Ok((at, directory(package, text, value.span)?))
        })
        .collect()
}

fn stage(target: Option<Value>) -> Result<Option<recipe::Stage>, Fault> {
    match target {
        Some(Value {
            kind: Kind::None, ..
        })
        | None => Ok(None),
        Some(target) => {
            let name = string("`image`'s `target`", target.span, &target)?;
            let stage = recipe::Stage::in_any_case(name);
            stage
                .map(Some)
                .map_err(|error| Fault::new(target.span, error))
        }
    }
}

fn named(contexts: Option<&Value>) -> Result<BTreeMap<String, ImageId>, Fault> {
    let entries = keyed(
        "`image`'s `contexts`",
        contexts,
        "images",
        "the name the recipe gives it",
    )?;
    entries
        .map(|entry| {
            let (key, Entry { value, .. }) = entry?;
            let Kind::Image(image) = value.kind else {
                let message = format!(
                    "a value of `image`'s `contexts` can't be {}: give an image, as `image()` \
                     makes",
                    value.shown()
                );
                return Err(Fault::new(value.span, message));
            };
            Ok((String::from(key), image))
        })
        .collect()
}

fn build_args(args: Option<&Value>) -> Result<BTreeMap<String, String>, Fault> {
    let entries = keyed(
        "`image`'s `args`",
        args,
        "strings",
        "its build argument's name",
    )?;
    entries
        .map(|entry| {
            let (key, Entry { value, .. }) = entry?;
            let text = string("a value of `image`'s `args`", value.span, value)?;
            Ok((String::from(key), String::from(text)))
        })
        .collect()
}

// The directory that `text` names, as `image` is given one: `.` is the entry file's, a path is
// below it, `//` starts one at the root, and `@pumpkin//` one of the standard library.
fn directory(package: &str, text: &str, at: Span) -> Result<Label, Fault> {
    // A root's name, which a component can't start with.
    if text.starts_with('@') && text.contains("//") {
        return Label::parse(text).map_err(|message| Fault::new(at, message));
    }
    let below = match (text, text.strip_prefix("//")) {
        (".", _) => String::from(package),
        (_, Some(rooted)) => String::from(rooted),
        (_, None) if package.is_empty() => String::from(text),
        (_, None) => format!("{package}/{text}"),
    };
    Label::below(Root::Task, below).ok_or_else(|| {
        let message = format!(
            "`{}` names no directory below the root: write `.` for the entry file's directory, a \
             path below it, or `//` and a path below the root, each in `/`-separated components \
             of ASCII letters, digits, `.`, `_` and `-`, none of them empty, `.` or `..`",
            escape(text)
        );
        Fault::new(at, message)
    })
}

// A path below the context, as a recipe's or a mount's.
fn in_context(text: &str, at: Span) -> Result<RelativePath, Fault> {
    let path = RelativePath::try_from(String::from(text)).ok();
    let path = path.filter(|path| path.as_str().split('/').all(is_component));
    path.ok_or_else(|| {
        let message = format!(
            "`{}` names no path in the context: write `/`-separated components of ASCII letters, \
             digits, `.`, `_` and `-`, none of them empty, `.` or `..`",
            escape(text)
        );
        Fault::new(at, message)
    })
}

// The first component of an image's repository: `pumpkin` for a task's directories, which tells
// its images from others on the engine, and `pumpkin-library` for the standard library's, so that
// no task's directory names the repository of one of them.
const REPOSITORIES: &str = "pumpkin";

const LIBRARY_REPOSITORIES: &str = "pumpkin-library";

// Docker's longest repository name, in characters:
// https://github.com/distribution/distribution/blob/f49f48736337e8fc9f7816a044819b0f7967d557/vendor/github.com/distribution/reference/reference.go#L38-L39
const MAX_REPOSITORY: usize = 255;

// A repository name is made of its directories, so each must be a component of Docker's grammar:
// lowercase letters and digits, joined by one `.`, one or two `_`, or dashes:
// https://github.com/distribution/distribution/blob/f49f48736337e8fc9f7816a044819b0f7967d557/vendor/github.com/distribution/reference/regexp.go#L42-L52
fn names_repository(directory: &str, at: Span) -> Result<(), Fault> {
    let alphanumeric = |char: char| char.is_ascii_lowercase() || char.is_ascii_digit();
    let names = |component: &str| {
        component.starts_with(alphanumeric)
            && component.ends_with(alphanumeric)
            && (component.split(alphanumeric)).all(|separator| {
                matches!(separator, "." | "_" | "__") || separator.bytes().all(|byte| byte == b'-')
            })
    };
    let Some(unnamed) = directory.split('/').find(|component| !names(component)) else {
        return Ok(());
    };
    let message = format!(
        "`{}`, a directory that holds the recipe, can't name a part of the image's Docker \
         repository, which the recipe's directory names: name it with lowercase letters and \
         digits, in runs joined by one `.`, one or two `_`, or dashes",
        escape(unnamed)
    );
    Err(Fault::new(at, message))
}

fn fits(repository: String, at: Span) -> Result<String, Fault> {
    // The repository is ASCII, as each of its parts is.
    let length = repository.len();
    if length <= MAX_REPOSITORY {
        return Ok(repository);
    }
    let message = format!(
        "the image's repository, `{}`, has {length} characters, more than Docker's \
         {MAX_REPOSITORY}: name the directory of the recipe with fewer",
        escape(&repository)
    );
    Err(Fault::new(at, message))
}

// `package_name()`: the entry file's directory below the root, such as `tasks/bench`, or the empty
// string for an entry at the root.
fn package_name(call: &Call<'_>, site: &Site<'_>) -> Result<Value, Fault> {
    const PARAMS: Params<0, 0, 0> = Params {
        positional: [],
        required: [],
        optional: [],
    };
    PARAMS.bind(Pumpkin::PackageName, call)?;
    let package = site.in_entry(Pumpkin::PackageName, call.at)?;
    Ok(Value {
        kind: Kind::Str(Text::new(package, call.at, call.budget)?),
        span: call.at,
    })
}
