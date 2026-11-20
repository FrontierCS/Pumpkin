//! Pumpkin's workflow language: a subset of Starlark, the language of Bazel's files, that
//! [`evaluate`] runs to a [`Plan`]: the images to build, and the workflow that names them.
//!
//! The subset only refuses, with one addition the Starlark specification
//! (<https://github.com/bazelbuild/starlark/blob/master/spec.md>) lacks: type annotations on a
//! `def`'s parameters and return, which starlark-rust
//! (<https://github.com/facebook/starlark-rust/blob/85e33edead0189db7a8312005ae8175a30bf155c/docs/types.md#L1-L25>)
//! and Bazel
//! (<https://github.com/bazelbuild/bazel/blob/a765dc177f48efb4c9e1b980609b838c9ba27f42/src/main/java/com/google/devtools/build/lib/packages/semantics/BuildLanguageOptions.java#L663-L675>)
//! accept.
//! An annotation only refuses too, where a value doesn't fit it, so a file the subset
//! accepts, with its annotations dropped, is valid Starlark and means what the specification
//! says it does. The workflow is what the entry file's `workflow()` call gives.
//! `eval` runs `pumpkin`'s functions, which record what an evaluation gives in `plan`'s types,
//! and `plan` reads it through `ir`: none uses one before it.
//!
//! An evaluation is hermetic: it reads source only through its [`Loader`], by [`Label`]s, which
//! stay below their root, and its `--arg`s ([`Args`]); and the crate is `no_std`, so that its own
//! code can name no file, clock, environment, process or randomly seeded hash, though `schema`'s
//! I/O items stay reachable through it. An
//! image is only declared, for its caller to build. The same sources and args always give the
//! same plan. An evaluation always ends, within [`STACK_BYTES`] of stack and its [`Bounds`], once
//! each method of the [`Host`] that it calls has returned; how its problems read is [`Problems`]'
//! doc's, and a linter's findings in the files it read read so too ([`Plan::lint`]). A host calls
//! a body through [`call`], whose handle alone reaches the [`Host`].

#![no_std]

extern crate alloc;

mod builtins;
mod eval;
mod ir;
mod json;
mod plan;
mod problem;
mod pumpkin;
mod syntax;
mod value;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

pub use eval::body::{Body, BodyKind, Host, Run, Target};
pub use plan::{Declared, Plan, Recipe};
pub use problem::{Finding, Problems};
pub use schema::ir::{Label, Root};

use crate::eval::Purpose;
use crate::plan::Planned;

/// How deep a file's syntax, an evaluation, and the values it makes may nest: brackets, calls,
/// indexes, unary operators, conditional expressions, comprehensions' clauses and blocks, in a
/// file and through the calls an evaluation is in; and lists, tuples, dicts and functions'
/// defaults in a value.
// One more than the 127 levels that `serde_json` reads, as no IR nested deeper reaches a run; it
// also bounds the stack of every walk over a file's syntax or a value, a value's drop included.
const MAX_NESTING: usize = 128;

/// The stack that a thread calling [`evaluate`], [`check`] or [`call`] needs: on a smaller one, a
/// file that nests deep enough overflows it, which aborts the process.
// 2.4 times the most an evaluation was measured to take, about 3.3 MiB in a debug build and
// 1.2 MiB in an optimized one, so that frames may grow with the code or the toolchain. The tests'
// `bad/stack` nests in the shape measured deepest, so that growth past the margin aborts them.
pub const STACK_BYTES: usize = 8 << 20;

/// How much an evaluation may compute and make: it evaluates at most `steps` expressions, and
/// its values take at most `bytes`, as `value`'s budget counts them.
#[derive(Clone, Copy)]
pub struct Bounds {
    pub steps: u64,
    pub bytes: usize,
}

impl Bounds {
    // So that a file that would run for minutes or never end, as nested comprehensions can, or
    // would ask for gigabytes, as doubling a string or a list can, ends at once: about 1,000
    // times the steps the heaviest workflow of the task repository takes, 837, and 2,400 times
    // the bytes it makes, 113,229.
    pub const WORKFLOW: Self = Self {
        steps: 1_000_000,
        bytes: 256 << 20,
    };
}

/// Evaluates the entry file at `entry`, and the files it loads, each read through `loader` once,
/// given `args`, to its plan, within `bounds`. The calling thread needs a stack of
/// [`STACK_BYTES`].
///
/// # Errors
///
/// An entry of the standard library; a file's first syntax error, or every name it fails to
/// resolve; the first error while the files run, passing `bounds` among them; or, once they have
/// run, the entry calling no `workflow()`, and each of `args` that no `arg()` call declares.
pub fn evaluate(
    entry: &Label,
    loader: &mut impl Loader,
    args: &Args,
    bounds: Bounds,
) -> Result<Plan, Problems> {
    let budget = value::Budget::new(bounds.bytes);
    let planned = Planned::new(args);
    let package = entry.path().rsplit_once('/').map_or("", |(dir, _)| dir);
    let purpose = Purpose::Workflow {
        package: String::from(package),
    };
    let mut evaluation = eval::Evaluation::new(loader, purpose, planned, bounds.steps, &budget);
    if let Err(problems) = evaluation.entry(entry) {
        return Err(Problems::new(evaluation.sources, problems));
    }
    Plan::new(entry, evaluation.sources, evaluation.planned)
}

/// Checks, without calling it, that `body` takes a call of `kind`, on an evaluation of its file,
/// each file read through `loader` once, given `args`, within `bounds`, on a [`STACK_BYTES`] stack.
///
/// # Errors
///
/// An evaluation's problem; no function `body.def` in the file; or, in source order, each parameter
/// that doesn't fit: a default or incorrect annotation on the handle, payload or submissions;
/// an extra positional parameter in a final body, or one not annotated `str`, `int` or `bool`; one
/// without a default that `body.keywords` misses; each keyword that names none, or doesn't fit.
pub fn check(
    body: &Body<'_>,
    kind: BodyKind,
    loader: &mut impl Loader,
    args: &Args,
    bounds: Bounds,
) -> Result<(), Problems> {
    let budget = value::Budget::new(bounds.bytes);
    let planned = Planned::new(args);
    let purpose = Purpose::Check;
    let mut evaluation = eval::Evaluation::new(loader, purpose, planned, bounds.steps, &budget);
    match evaluation.body(body, kind) {
        Ok(_) => Ok(()),
        Err(problems) => Err(Problems::new(evaluation.sources, problems)),
    }
}

/// Calls the body that `body` names, on an evaluation of its file as [`check`] has it.
///
/// The second argument is the payload for a job or the per-role submissions for a final call.
/// The returned dictionary gives each name with its JSON value.
///
/// # Errors
///
/// What [`check`] refuses; an argument that names no parameter after the payload, or one without a
/// default not given; the body's first error; a return that is no dict of names to JSON values.
pub fn call(
    body: &Body<'_>,
    host: Host<'_>,
    arguments: &BTreeMap<schema::ir::Name, schema::ir::ArgumentValue>,
    loader: &mut impl Loader,
    args: &Args,
    bounds: Bounds,
) -> Result<Vec<(schema::ir::Name, serde_json::Value)>, Problems> {
    let budget = value::Budget::new(bounds.bytes);
    let planned = Planned::new(args);
    let kind = host.kind();
    let purpose = Purpose::Call(host);
    let mut evaluation = eval::Evaluation::new(loader, purpose, planned, bounds.steps, &budget);
    let returned = match evaluation.body(body, kind) {
        Ok(read) => (evaluation.returned(&read, arguments)).map_err(Vec::from),
        Err(problems) => Err(problems),
    };
    returned.map_err(|problems| Problems::new(evaluation.sources, problems))
}

/// Where an evaluation reads its source, below either [`Root`].
pub trait Loader {
    /// The text of the file at `label`, or why it can't be read, which the evaluation reports at
    /// the `load` that names it, or as the first file's problem.
    fn load(&mut self, label: &Label) -> Result<String, String>;
}

/// The `--arg`s given to an evaluation, each the text of a value that an `arg()` call reads, by
/// its name.
#[derive(Debug)]
pub struct Args(BTreeMap<String, String>);

impl Args {
    /// The `--arg`s that `given` writes, as [`schema::ir::argument::texts`] reads them.
    ///
    /// # Errors
    ///
    /// What [`schema::ir::argument::texts`] refuses.
    pub fn parse<'g>(given: impl IntoIterator<Item = &'g str>) -> Result<Self, Vec<String>> {
        schema::ir::argument::texts(given).map(Self)
    }

    /// Each arg's name and value, sorted by name.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        (self.0.iter()).map(|(name, value)| (name.as_str(), value.as_str()))
    }
}
