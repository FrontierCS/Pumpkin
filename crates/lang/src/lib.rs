//! Pumpkin's workflow language: a subset of Starlark, the language of Bazel's files, that
//! [`evaluate`] runs to a [`Plan`]: the images to build, and the workflow that names them.
//!
//! The subset only refuses: a program it accepts means what the Starlark specification
//! (<https://github.com/bazelbuild/starlark/blob/master/spec.md>) says it does, so every file it
//! accepts is valid Starlark. The workflow is what the entry file's `workflow()` call gives.
//! `eval` runs `pumpkin`'s functions, which record what an evaluation gives in `plan`'s types,
//! and `plan` reads it through `ir`: none uses one before it.
//!
//! An evaluation is hermetic: it reads source only through its [`Loader`], by [`Label`]s, which
//! stay below their root, and its `--arg`s ([`Args`]); and the crate is `no_std`, so that its own
//! code can name no file, clock, environment, process or randomly seeded hash, though `schema`'s
//! I/O items stay reachable through it. An
//! image is only declared, for its caller to build. The same sources and args always give the
//! same plan. An evaluation always ends, within [`STACK_BYTES`] of stack and its [`Bounds`]; how
//! its problems read is [`Problems`]' doc's, and a linter's findings in the files it read read so
//! too ([`Plan::lint`]).

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
use alloc::collections::btree_map::Entry as Slot;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

pub use plan::{Declared, Plan, Recipe};
pub use problem::{Finding, Problems};

use crate::plan::Planned;

/// How deep a file's syntax, an evaluation, and the values it makes may nest: brackets, calls,
/// indexes, unary operators, conditional expressions, comprehensions' clauses and blocks, in a
/// file and through the calls an evaluation is in; and lists, tuples, dicts and functions'
/// defaults in a value.
// One more than the 127 levels that `serde_json` reads, as no IR nested deeper reaches a run; it
// also bounds the stack of every walk over a file's syntax or a value, a value's drop included.
const MAX_NESTING: usize = 128;

/// The stack that a thread calling [`evaluate`] needs: on a smaller one, a file that nests deep
/// enough overflows it, which aborts the process.
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
    let planned = Planned::new(entry, args);
    let mut evaluation = eval::Evaluation::new(loader, planned, bounds.steps, &budget);
    if let Err(problems) = evaluation.entry(entry) {
        return Err(Problems::new(evaluation.sources, problems));
    }
    Plan::new(entry, evaluation.sources, evaluation.planned)
}

/// Where an evaluation reads its source, below either [`Root`].
pub trait Loader {
    /// The text of the file at `label`, or why it can't be read, which the evaluation reports at
    /// the `load` that names it, or as the entry's problem.
    fn load(&mut self, label: &Label) -> Result<String, String>;
}

/// The `--arg`s given to an evaluation, each the text of a value that an `arg()` call reads, by
/// its name.
#[derive(Debug)]
pub struct Args(BTreeMap<String, String>);

impl Args {
    /// The `--arg`s that `given` writes, each as `<name>=<value>`: its name is its text before its
    /// first `=`, and its value the rest, whatever it holds.
    ///
    /// # Errors
    ///
    /// What is wrong with each one that has no `=`, has no name, or names an arg given before it.
    pub fn parse<'g>(given: impl IntoIterator<Item = &'g str>) -> Result<Self, Vec<String>> {
        let mut args = BTreeMap::new();
        let mut problems = Vec::new();
        for text in given {
            let problem = match text.split_once('=') {
                None => "has no `=`: give it as `--arg <name>=<value>`",
                Some(("", _)) => "names no arg before its `=`: give it as `--arg <name>=<value>`",
                Some((name, value)) => match args.entry(String::from(name)) {
                    Slot::Vacant(slot) => {
                        slot.insert(String::from(value));
                        continue;
                    }
                    Slot::Occupied(_) => "names an arg given already: give each arg once",
                },
            };
            problems.push(format!("`--arg {}` {problem}", text.escape_debug()));
        }
        if problems.is_empty() {
            Ok(Self(args))
        } else {
            Err(problems)
        }
    }

    /// Each arg's name and value, sorted by name.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        (self.0.iter()).map(|(name, value)| (name.as_str(), value.as_str()))
    }
}

/// A place below a [`Root`], which names no place outside it.
///
/// It is `//`, the task's root, or `@pumpkin//`, the standard library's, then `/`-separated
/// components of ASCII letters, digits, `.`, `_` and `-`, none of them empty, `.` or `..`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Label {
    root: Root,
    path: String,
}

/// The root that a [`Label`] names a place below. A [`Loader`] reads the files below both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Root {
    /// The root of the task's repository, which the entry is below.
    Task,
    /// Pumpkin's standard library, whose files load only files of its own, so that it depends
    /// on no task.
    Library,
}

/// What starts a label of the standard library, [`Root::Library`].
// As Bazel names a repository's root, `@rules_oci//` (https://bazel.build/concepts/labels), and
// Buck2 the cell of its bundled prelude, `prelude//`:
// https://github.com/facebook/buck2/blob/e3dc98f84acf5f70c40b857819556baa7ec9c929/docs/users/advanced/external_cells.md#L50-L56
const LIBRARY: &str = "@pumpkin//";

impl Label {
    /// Parses a label, such as `//lib/bench.star` or `@pumpkin//harness/codex/codex.star`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (root, path) = if let Some(path) = text.strip_prefix(LIBRARY) {
            (Root::Library, path)
        } else if let Some(path) = text.strip_prefix("//") {
            (Root::Task, path)
        } else if text.starts_with('@') && text.contains("//") {
            return Err(format!(
                "`{}` names a root Pumpkin has none of: start it with `//`, the task's root, or \
                 `{LIBRARY}`, the standard library's",
                text.escape_debug()
            ));
        } else {
            return Err(format!(
                "`{}` is no label: start it with `//`, the root",
                text.escape_debug()
            ));
        };
        Self::below(root, String::from(path)).ok_or_else(|| {
            let start = &text[..text.len() - path.len()];
            format!(
                "`{}` is no label: after `{start}`, write `/`-separated components of ASCII \
                 letters, digits, `.`, `_` and `-`, none of them empty, `.` or `..`",
                text.escape_debug()
            )
        })
    }

    pub(crate) fn below(root: Root, path: String) -> Option<Self> {
        (path.split('/').all(is_component)).then_some(Self { root, path })
    }

    pub fn root(&self) -> Root {
        self.root
    }

    /// The path below its root, such as `lib/bench.star`.
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let start = match self.root {
            Root::Task => "//",
            Root::Library => LIBRARY,
        };
        write!(f, "{start}{}", self.path)
    }
}

fn is_component(component: &str) -> bool {
    !matches!(component, "" | "." | "..")
        && (component.chars())
            .all(|char| char.is_ascii_alphanumeric() || matches!(char, '.' | '_' | '-'))
}
