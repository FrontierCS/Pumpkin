//! Pumpkin's predeclared names: [`Pumpkin`]'s functions, which make what an evaluation gives
//! ([`crate::plan`]), its workflow, its images and the `--arg`s ([`Args`](crate::Args)) it
//! reads; and the units `KiB`, `MiB` and `GiB`, the ints 2^10, 2^20 and 2^30.
//!
//! Each function takes its arguments as a `def` of its signature would. All but `arg()` refuse a
//! call made while a file other than the entry loads ([`Site::in_entry`]).

use alloc::collections::BTreeMap;
use alloc::collections::btree_map::Entry as Slot;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroU64;

use harness::codex::{self, CODEX_0_167_0, ReasoningEffort, Tools, Workspace};
use schema::ir::{self, AbsolutePath, ModelName};
use schema::sandbox::RelativePath;
use serde::Serialize;
use serde_json::Value as Json;

use crate::builtins::{Call, Universal, called, lacks, place, string};
use crate::plan::{Image, Place, Planned, Recipe, WorkflowCall, declare};
use crate::problem::Fault;
use crate::syntax::{Def, is_name};
use crate::value::{
    Budget, Builtin, Dict, Entry, ImageId, Key, Kind, Nested, Span, Text, Value, escape, quote,
};
use crate::{Label, is_component};

pub(crate) fn predeclared(name: &str) -> Option<Kind> {
    let kind = match name {
        "KiB" => Kind::Int(1 << 10),
        "MiB" => Kind::Int(1 << 20),
        "GiB" => Kind::Int(1 << 30),
        _ => {
            let pumpkin = Pumpkin::ALL
                .into_iter()
                .find(|pumpkin| pumpkin.name() == name)?;
            Kind::Builtin(Builtin::Pumpkin(pumpkin))
        }
    };
    Some(kind)
}

/// Pumpkin's own predeclared functions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pumpkin {
    Workflow,
    Arg,
    Image,
    Codex,
    PackageName,
}

impl Pumpkin {
    const ALL: [Self; 5] = [
        Self::Workflow,
        Self::Arg,
        Self::Image,
        Self::Codex,
        Self::PackageName,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Workflow => "workflow",
            Self::Arg => "arg",
            Self::Image => "image",
            Self::Codex => "codex",
            Self::PackageName => "package_name",
        }
    }

    /// What `call` of this function, made at `site`, gives, adding what it plans to `planned`.
    ///
    /// # Errors
    ///
    /// As the function's own doc says.
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
            Self::Codex => codex(call, site, planned),
            Self::PackageName => package_name(call, site, planned),
        }
    }
}

/// Where a call of one of Pumpkin's functions is made.
pub(crate) struct Site<'e> {
    /// The module whose statements run.
    pub(crate) module: &'e Label,
    /// Whether that module is the entry file.
    pub(crate) entry: bool,
    /// The functions that run, outermost first, each by its `def`, with the call that runs it.
    pub(crate) calls: &'e [(Rc<Def>, Span)],
}

impl Site<'_> {
    /// Refuses the call of `function`, at `at`, unless the entry file's statements run, whatever
    /// function makes it, as no file may depend on what loads it (spec.md, "Module execution").
    fn in_entry(&self, function: Pumpkin, at: Span) -> Result<(), Fault> {
        if self.entry {
            return Ok(());
        }
        let (function, module) = (function.name(), self.module);
        let message = format!(
            "`{function}()` is called while `{module}` loads: call it in the entry file, which \
             loads `{module}`"
        );
        Err(Fault::new(at, message))
    }

    /// The place of the call at `at`, in the calls that run.
    fn place(&self, at: Span) -> Place {
        let calls = self.calls.iter().rev();
        Place {
            at,
            calls: calls.map(|(def, at)| (called(def), *at)).collect(),
        }
    }
}

/// The parameters of one of Pumpkin's functions, by name: `P` that a call may give by position
/// or by keyword, then `R` and `O` that it gives by keyword only; each of the `P` and the `R` is
/// required.
struct Params<const P: usize, const R: usize, const O: usize> {
    positional: [&'static str; P],
    required: [&'static str; R],
    optional: [&'static str; O],
}

/// The arguments a call gives [`Params`], in their order.
struct Bound<const P: usize, const R: usize, const O: usize> {
    positional: [Value; P],
    required: [Value; R],
    optional: [Option<Value>; O],
}

impl<const P: usize, const R: usize, const O: usize> Params<P, R, O> {
    /// The arguments that `call` gives `function`, whose parameters these are.
    ///
    /// # Errors
    ///
    /// As [`place`]'s, or a required argument that the call doesn't give.
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

/// `workflow(*, name, roles, score)`: the evaluation's workflow is the one whose IR has these
/// fields, at the version this crate reads. It returns `None`.
///
/// # Errors
///
/// A call made while a file other than the entry loads; a second call, where `planned` holds the
/// first; or arguments that nest too deep in the dict of them.
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
    let fields = (call.keyword.iter())
        .map(|(name, value)| {
            Ok(Entry {
                key: Key::Str(Text::new(&name.text, name.span, call.budget)?),
                key_span: name.span,
                value: value.clone(),
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

/// `arg(name, type, *, default)`: the value of the `--arg` named `name`, its text read as `type`,
/// the builtin `str`, `int` or `bool`, reads it: whole, in decimal as `int` reads a string, or
/// `true` or `false`; or else `default`, of that type or `None`. A value given is made at the call,
/// and a default keeps its place. No `--arg` gives `None`: an arg is `None` only left out.
///
/// # Errors
///
/// A name no Starlark name, another type or a default of another; an arg declared already; or a
/// value `type` can't read, or none without a default, noting each `--arg` no `arg()` declared.
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

/// The type of an arg's value, which `arg()`'s `type` names by its builtin.
#[derive(Clone, Copy)]
enum Type {
    Str,
    Int,
    Bool,
}

impl Type {
    /// The type that `value`, given as `arg()`'s `type`, names.
    fn of(value: &Value) -> Result<Self, Fault> {
        let kind = match &value.kind {
            Kind::Builtin(Builtin::Universal(universal)) => match universal {
                Universal::Str => Some(Self::Str),
                Universal::Int => Some(Self::Int),
                Universal::Bool => Some(Self::Bool),
                Universal::Fail | Universal::Len | Universal::Sorted => None,
            },
            Kind::None
            | Kind::Bool(_)
            | Kind::Int(_)
            | Kind::Str(_)
            | Kind::List(_)
            | Kind::Tuple(_)
            | Kind::Dict(_)
            | Kind::Function(_)
            | Kind::Builtin(Builtin::Pumpkin(_))
            | Kind::Image(_) => None,
        };
        kind.ok_or_else(|| {
            let message = format!(
                "`arg`'s `type` can't be {}: give the builtin `str`, `int` or `bool`",
                value.shown()
            );
            Fault::new(value.span, message)
        })
    }

    /// The builtin that names the type, and a value of it, for a message.
    fn words(self) -> (&'static str, &'static str) {
        match self {
            Self::Str => ("str", "a string"),
            Self::Int => ("int", "an int"),
            Self::Bool => ("bool", "a bool"),
        }
    }

    /// `default`, as the default of an arg of this type.
    fn default(self, default: Value) -> Result<Value, Fault> {
        // `None` leaves the program to pick the value of an arg not given, from what it knows,
        // such as a task's own values.
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

    /// The value of this type that `text`, given as `--arg <name>=<text>`, is, made at `at`,
    /// counted in `budget`.
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

/// `image(recipe, *, context = ".", mount = {}, target = None, args = {}, contexts = {})`: the
/// [`Recipe`]'s image that the Dockerfile at `recipe`, in `context`, builds, with each directory
/// of `mount` at its path in the context, and its stage `target`, in any case, given each string
/// of `args` and image of `contexts` as the build argument and the named context of its key.
///
/// # Errors
///
/// A call made while a file other than the entry loads; a directory that names none below the
/// root, or can't name a Docker repository; or another argument that isn't as above.
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
    site.in_entry(Pumpkin::Image, at)?;
    let file = string("`image`'s `recipe`", recipe.span, &recipe)?;
    let file = in_context(file, recipe.span)?;
    let (context, context_at) = match context {
        Some(context) => {
            let text = string("`image`'s `context`", context.span, &context)?;
            (
                directory(&planned.package, text, context.span)?,
                context.span,
            )
        }
        None if planned.package.is_empty() => {
            let message = "the context defaults to the entry file's directory, the root, which \
                           can't be a context: give `context` a directory below it";
            return Err(Fault::new(at, message));
        }
        None => (directory(&planned.package, ".", at)?, at),
    };
    let mounts = mounts(&planned.package, mount.as_ref())?;
    let target = stage(target)?;
    let args = build_args(args.as_ref())?;
    let contexts = named(contexts.as_ref())?;
    names_repository(context.path(), context_at)?;
    let repository = match file.as_str().rsplit_once('/') {
        Some((dir, _)) => {
            names_repository(dir, recipe.span)?;
            format!("{REPOSITORIES}/{}/{dir}", context.path())
        }
        None => format!("{REPOSITORIES}/{}", context.path()),
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
    let image = Image::Recipe { recipe, contexts };
    let image = declare(&mut planned.images, image, site.place(at));
    Ok(Value {
        kind: Kind::Image(image),
        span: at,
    })
}

/// The entries of `dict`, `image()`'s argument `what`, if given, which gives a dict of `values`,
/// each by `key`, a string: each entry, with its key.
///
/// # Errors
///
/// `dict` is no dict, or, as the iterator reaches it, a key is no string.
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
        Key::Int(_) => {
            let message = format!(
                "{what} can't key a value by `{}`: key it by {key}, a string",
                entry.key.literal()
            );
            Err(Fault::new(entry.key_span, message))
        }
    }))
}

/// The directories that `mount`, `image()`'s, mounts in the context, each by its path there.
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

/// The stage that `target`, `image()`'s, names in any case, if any.
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

/// The images that `contexts`, `image()`'s, gives as named contexts, each by its key.
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

/// The build arguments that `args`, `image()`'s, gives, each string by its key.
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

/// The directory that `text`, a path written at `at`, names in the package `package`: `.` is the
/// package, a path below it is relative to it, and `//` starts one below the root.
fn directory(package: &str, text: &str, at: Span) -> Result<Label, Fault> {
    let below = match (text, text.strip_prefix("//")) {
        (".", _) => String::from(package),
        (_, Some(rooted)) => String::from(rooted),
        (_, None) if package.is_empty() => String::from(text),
        (_, None) => format!("{package}/{text}"),
    };
    Label::below(below).ok_or_else(|| {
        let message = format!(
            "`{}` names no directory below the root: write `.` for the entry file's directory, a \
             path below it, or `//` and a path below the root, each in `/`-separated components \
             of ASCII letters, digits, `.`, `_` and `-`, none of them empty, `.` or `..`",
            escape(text)
        );
        Fault::new(at, message)
    })
}

/// The path that `text`, written at `at`, names in an image's context.
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

/// The repository below which `image()`'s images are built, so that they are told apart from
/// other images on the engine.
const REPOSITORIES: &str = "pumpkin";

/// The most characters that a Docker repository's name, below its registry, may hold.
// `RepositoryNameTotalLengthMax` in Docker's reference grammar.
const MAX_REPOSITORY: usize = 255;

/// Refuses `directory`, a path of the directories that hold a recipe, written at `at`, unless
/// each of them can name a part of a Docker repository, as the recipe's image's repository is
/// the recipe's directory below the root, below [`REPOSITORIES`].
fn names_repository(directory: &str, at: Span) -> Result<(), Fault> {
    // Docker's reference grammar's `path-component`: lowercase letters and digits, in runs
    // joined by one `.`, one or two `_`, or any number of dashes.
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

/// `repository`, the repository of the image declared at `at`, unless it is too long for Docker
/// to name.
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

/// `codex(base, *, model, effort, tools, workspace, prompt, deadline_seconds,
/// finalize_grace_seconds)`: what a role of an agent running Codex takes: `image`, `base` with
/// Codex's layer, [`CODEX_0_167_0`]; `runtime`, [`codex::runtime`]'s for `prompt`, Codex in
/// `workspace` on `model` at `effort`, with `tools`, deadline and grace; `egress`, `model`'s route.
///
/// # Errors
///
/// A call made while a file other than the entry loads; a base that is no image; or a value that
/// the harness, its adapter or the IR refuses, at that value.
fn codex(call: &Call<'_>, site: &Site<'_>, planned: &mut Planned<'_>) -> Result<Value, Fault> {
    const PARAMS: Params<1, 7, 0> = Params {
        positional: ["base"],
        required: [
            "model",
            "effort",
            "tools",
            "workspace",
            "prompt",
            "deadline_seconds",
            "finalize_grace_seconds",
        ],
        optional: [],
    };
    let Bound {
        positional: [base],
        required,
        optional: [],
    } = PARAMS.bind(Pumpkin::Codex, call)?;
    let at = call.at;
    site.in_entry(Pumpkin::Codex, at)?;
    let Kind::Image(on) = base.kind else {
        let message = format!(
            "`codex`'s `base` can't be {}: give an image, as `image()` makes",
            base.shown()
        );
        return Err(Fault::new(base.span, message));
    };
    let [runtime, egress] = agent(&required, call)?;
    let base_repository = planned.images[on.0].image.repository();
    let repository = format!("{base_repository}/{}", CODEX_0_167_0.name);
    let layer = Image::Layer {
        base: on,
        pin: CODEX_0_167_0,
        repository: fits(repository, base.span)?,
    };
    let layer = declare(&mut planned.images, layer, site.place(at));
    let image = Value {
        kind: Kind::Image(layer),
        span: at,
    };
    record(
        [("image", image), ("runtime", runtime), ("egress", egress)],
        call,
    )
}

/// The runtime of an agent that runs Codex and its route, as `codex()`'s doc says, from the
/// call's `model`, `effort`, `tools`, `workspace`, `prompt`, `deadline_seconds` and
/// `finalize_grace_seconds`, made by `call`.
fn agent(
    [model, effort, tools, workspace, prompt, deadline, grace]: &[Value; 7],
    call: &Call<'_>,
) -> Result<[Value; 2], Fault> {
    // The harness's and the IR's own errors, at the value they reject.
    let rejected = |value: &Value| {
        let at = value.span;
        move |message: String| Fault::new(at, message)
    };
    let name = string("`codex`'s `model`", model.span, model)?;
    let model_name = ModelName::try_from(String::from(name)).map_err(rejected(model))?;
    let effort_text = string("`codex`'s `effort`", effort.span, effort)?;
    let reasoning = ReasoningEffort::try_from(effort_text).map_err(rejected(effort))?;
    let tools_text = string("`codex`'s `tools`", tools.span, tools)?;
    let sent = Tools::try_from(tools_text).map_err(rejected(tools))?;
    let path = string("`codex`'s `workspace`", workspace.span, workspace)?;
    let place = AbsolutePath::try_from(String::from(path)).and_then(Workspace::try_from);
    let place = place.map_err(rejected(workspace))?;
    let text = string("`codex`'s `prompt`", prompt.span, prompt)?;
    let deadline_seconds = seconds("`codex`'s `deadline_seconds`", deadline)?;
    let Some(deadline_seconds) = NonZeroU64::new(deadline_seconds) else {
        let message = "`codex`'s `deadline_seconds` is 0: give a positive int";
        return Err(Fault::new(deadline.span, message));
    };
    let grace_seconds = seconds("`codex`'s `finalize_grace_seconds`", grace)?;
    // The adapter refuses only a model whose tools it can't send as `tools` says.
    let runtime = codex::runtime(
        place,
        &model_name,
        reasoning,
        sent,
        text,
        deadline_seconds,
        grace_seconds,
    );
    let runtime = ir::Runtime::Agent(runtime.map_err(rejected(model))?);
    let (at, budget) = (call.at, call.budget);
    let made = |kind| Value { kind, span: at };
    let name = made(Kind::Str(Text::new(name, at, budget)?));
    let models = Nested::new(Vec::from([name]), at, budget)?;
    let api = made(Kind::Str(Text::new("responses", at, budget)?));
    let entry = record([("api", api), ("models", made(Kind::List(models)))], call)?;
    let route = made(Kind::List(Nested::new(Vec::from([entry]), at, budget)?));
    Ok([data(&runtime, call)?, record([("model", route)], call)?])
}

/// `package_name()`: the entry file's directory below the root, such as `tasks/bench`, or the
/// empty string for an entry at the root.
///
/// # Errors
///
/// A call given an argument, or made while a file other than the entry loads.
fn package_name(
    call: &Call<'_>,
    site: &Site<'_>,
    planned: &mut Planned<'_>,
) -> Result<Value, Fault> {
    const PARAMS: Params<0, 0, 0> = Params {
        positional: [],
        required: [],
        optional: [],
    };
    PARAMS.bind(Pumpkin::PackageName, call)?;
    site.in_entry(Pumpkin::PackageName, call.at)?;
    Ok(Value {
        kind: Kind::Str(Text::new(&planned.package, call.at, call.budget)?),
        span: call.at,
    })
}

/// The dict of `fields`, each value by its key, made by `call`.
fn record<K: AsRef<str>>(
    fields: impl IntoIterator<Item = (K, Value)>,
    call: &Call<'_>,
) -> Result<Value, Fault> {
    let at = call.at;
    let entries = fields.into_iter().map(|(key, value)| {
        Ok(Entry {
            key: Key::Str(Text::new(key.as_ref(), at, call.budget)?),
            key_span: at,
            value,
        })
    });
    let entries = entries.collect::<Result<_, Fault>>()?;
    let kind = Kind::Dict(Dict::new(entries, at, call.budget)?);
    Ok(Value { kind, span: at })
}

/// The seconds that `value`, given as `what`, is: an int of 0 or more.
fn seconds(what: &str, value: &Value) -> Result<u64, Fault> {
    let Kind::Int(int) = value.kind else {
        let message = format!("{what} can't be {}: give an int", value.shown());
        return Err(Fault::new(value.span, message));
    };
    u64::try_from(int).map_err(|_| {
        let message = format!("{what} is {int}: give an int of 0 or more");
        Fault::new(value.span, message)
    })
}

/// `data`, a part of the IR, as the values that write it in its JSON form, each made by `call`.
fn data(data: &impl Serialize, call: &Call<'_>) -> Result<Value, Fault> {
    match serde_json::to_value(data) {
        Ok(json) => from_json(json, call),
        Err(error) => unreachable!(
            "the IR doesn't serialize, though its types serialize to JSON, as every run's record \
             writes them: {error}"
        ),
    }
}

/// `json`, as the values that write it, each made by `call`.
fn from_json(json: Json, call: &Call<'_>) -> Result<Value, Fault> {
    let at = call.at;
    let kind = match json {
        Json::Null => Kind::None,
        Json::Bool(bool) => Kind::Bool(bool),
        Json::Number(number) => {
            let Some(int) = number.as_i64() else {
                unreachable!(
                    "the IR holds {number}, which is no 64-bit int, though the only numbers in \
                     the parts of the IR that `codex()` writes, an agent's runtime and its route, \
                     are the ints its call gives"
                )
            };
            Kind::Int(int)
        }
        Json::String(text) => Kind::Str(Text::new(&text, at, call.budget)?),
        Json::Array(items) => {
            let items = items.into_iter().map(|item| from_json(item, call));
            let items = items.collect::<Result<_, _>>()?;
            Kind::List(Nested::new(items, at, call.budget)?)
        }
        Json::Object(fields) => {
            let fields = fields
                .into_iter()
                .map(|(key, value)| Ok((key, from_json(value, call)?)));
            return record(fields.collect::<Result<Vec<_>, Fault>>()?, call);
        }
    };
    Ok(Value { kind, span: at })
}
