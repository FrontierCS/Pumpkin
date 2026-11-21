//! The workflow language, through `lang::evaluate`, then the plan's `check` and `build`, case by
//! case: each case is a fixture beside its snapshot, as `Case::parse` reads it, which may load the
//! files of `tests/fixtures` that `fixtures` lists; and every prefix of every file a case holds
//! must evaluate or give problems that read, as `Case::prefixes` has it. The conformance suites of
//! `tests/conformance` run as cases too, chunk by chunk, as `conformance` has it.

#![expect(
    clippy::panic,
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

mod body;
mod conformance;
mod fixtures;

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::num::NonZeroUsize;
use std::path::Path;
use std::{fs, thread};

use expect_test::{Expect, expect};
use lang::{Args, Bounds, Declared, Label, Loader, Problems, Recipe, Root};
use schema::ir;
use schema::workflow::Workflow;

fn files() -> BTreeMap<String, String> {
    files_below("tests/fixtures")
}

/// Each file below `dir`, a directory of the crate's, by its path below it.
fn files_below(dir: &str) -> BTreeMap<String, String> {
    fn read(dir: &Path, below: &str, files: &mut BTreeMap<String, String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            let path = if below.is_empty() {
                name
            } else {
                format!("{below}/{name}")
            };
            if entry.file_type().unwrap().is_dir() {
                read(&entry.path(), &path, files);
            } else {
                files.insert(path, fs::read_to_string(entry.path()).unwrap());
            }
        }
    }
    let mut files = BTreeMap::new();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(dir);
    read(&root, "", &mut files);
    files
}

/// What a case evaluates: an entry, and the files it holds, which a loader reads before those
/// of `tests/fixtures`, within `bounds`.
struct Case<'c> {
    entry: &'c str,
    /// Each file, by its path below the root.
    held: Vec<(&'c str, &'c str)>,
    files: &'c BTreeMap<String, String>,
    bounds: Bounds,
}

impl<'c> Case<'c> {
    /// The case that `fixture` writes, over `files`, within `bounds`, and its `--arg`s, each
    /// `<name>=<value>`.
    /// After a newline that starts it, if one does, and dedented not at all, so that a file's
    /// columns are the editor's, the fixture has a line `#- --arg <name>=<value>` for each
    /// `--arg`, then a line `#- <label>` above each file, `//…` or `@pumpkin//…`, which holds the
    /// lines below it up to the next; with no such line, the rest is the file `//text.star`. The
    /// first file is the entry; a label with no line below it names one of `files` as the entry,
    /// the case holding none of its text.
    fn parse(
        fixture: &'c str,
        files: &'c BTreeMap<String, String>,
        bounds: Bounds,
    ) -> (Vec<&'c str>, Self) {
        let mut rest = fixture.strip_prefix('\n').unwrap_or(fixture);
        let mut args = Vec::new();
        while let Some(line) = rest.strip_prefix("#- --arg ") {
            let (arg, after) = line.split_once('\n').unwrap_or((line, ""));
            args.push(arg);
            rest = after;
        }
        // Each file's path, where its line `#- //<label>` starts, and where its text does.
        let mut heads = Vec::new();
        let mut at = 0;
        for line in rest.split_inclusive('\n') {
            if line.starts_with("#-") {
                let label = line.trim_end_matches('\n');
                let label = (label.strip_prefix("#- ")).unwrap_or_else(|| {
                    panic!("`{label}` is no line of a fixture: write `#- //<label>` above a file")
                });
                Label::parse(label).unwrap();
                // A task's file by its path, as `Sources` reads it, and the library's by its label.
                let path = label.strip_prefix("//").unwrap_or(label);
                heads.push((path, at, at + line.len()));
            }
            at += line.len();
        }
        let ends = heads.iter().skip(1).map(|&(_, line, _)| line);
        let ends = ends.chain([rest.len()]);
        let mut parsed: Vec<(&str, &str)> = (heads.iter().zip(ends))
            .map(|(&(path, _, start), end)| (path, &rest[start..end]))
            .collect();
        match heads.first() {
            None => parsed.push(("text.star", rest)),
            Some(&(_, 0, _)) => {}
            Some(_) => panic!("text is above a fixture's first `#- //<label>`: label its file"),
        }
        let entry = parsed[0].0;
        let held = parsed.into_iter().filter(|(_, text)| !text.is_empty());
        let held = held.collect();
        let case = Self {
            entry,
            held,
            files,
            bounds,
        };
        (args, case)
    }

    /// What the entry gives, given `args`: each image built, a line each as `run` describes it,
    /// then the IR; or else the problems, as they read.
    fn evaluate(&self, args: &Args) -> Result<String, String> {
        let mut sources = Sources {
            held: &self.held,
            files: self.files,
        };
        let mut built = Vec::new();
        let workflow = run(self.entry, &mut sources, args, self.bounds, &mut built);
        let workflow = workflow.map_err(|problems| problems.to_string())?;
        let ir = serde_json::to_value(workflow.ir()).unwrap();
        let built: String = built.iter().flat_map(|line| [line, "\n"]).collect();
        Ok(format!("{built}{ir:#}"))
    }

    /// How many prefixes of the file at `path` evaluate, given `args`, of how many it cuts: at
    /// every `every`th character's boundary, at each line's end and at the file's, with the other
    /// files whole; one whose problems don't read panics, named beside the panic.
    fn prefixes(&self, path: &str, args: &Args, every: usize) -> (usize, usize) {
        let held = self.held.iter().find(|(held, _)| *held == path);
        let text = held.map_or_else(|| self.files[path].as_str(), |(_, text)| text);
        let mut cut = Self {
            held: (self.held.iter())
                .filter(|(held, _)| *held != path)
                .copied()
                .collect(),
            ..*self
        };
        cut.held.push((path, text));
        let ends = (text.char_indices().enumerate())
            .filter(|&(index, (_, char))| index % every == 0 || char == '\n')
            .map(|(_, (end, _))| end)
            .chain([text.len()]);
        let (mut evaluated, mut cuts) = (0, 0);
        for end in ends {
            let _named = Named { path, end };
            *cut.held.last_mut().unwrap() = (path, &text[..end]);
            evaluated += usize::from(cut.evaluate(args).is_ok());
            cuts += 1;
        }
        (evaluated, cuts)
    }
}

/// The prefix of the file at `path` cut at `end`, which it names if its evaluation panics.
struct Named<'p> {
    path: &'p str,
    end: usize,
}

impl Drop for Named<'_> {
    #[expect(
        clippy::print_stderr,
        reason = "a test shows what it writes as a panic unwinds it"
    )]
    fn drop(&mut self) {
        if thread::panicking() {
            eprintln!("{}: the prefix of {} bytes panics", self.path, self.end);
        }
    }
}

/// The files a case holds, then those of `tests/fixtures`, as a loader reads them: a task's by its
/// path, and the standard library's, of which only the case holds any, by its label.
struct Sources<'s> {
    held: &'s [(&'s str, &'s str)],
    files: &'s BTreeMap<String, String>,
}

impl Loader for Sources<'_> {
    fn load(&mut self, label: &Label) -> Result<String, String> {
        let path = match label.root() {
            Root::Task => label.path().to_owned(),
            Root::Library => label.to_string(),
        };
        if let Some((_, text)) = self.held.iter().find(|(held, _)| **held == path) {
            return Ok((*text).to_owned());
        }
        (self.files.get(&path).cloned())
            .ok_or_else(|| format!("no fixture is at `{path}`: name a file in the fixtures"))
    }
}

/// What `show` gives, on a thread with the stack an evaluation needs; a panic on it fails the
/// test where its case is.
#[track_caller]
fn on_stack(show: impl FnOnce() -> String + Send) -> String {
    let shown = thread::scope(|scope| {
        // An overflow aborts the process: the thread's name says whose test it was.
        let thread = (thread::Builder::new())
            .name(thread::current().name().unwrap_or("check").to_owned())
            .stack_size(lang::STACK_BYTES);
        thread.spawn_scoped(scope, show).unwrap().join()
    });
    shown.unwrap()
}

#[track_caller]
fn check(fixture: &str, expect: &Expect) {
    check_within(Bounds::WORKFLOW, fixture, expect);
}

/// Checks what the case that `fixture` writes, as `Case::parse` reads it, within `bounds`, shows
/// against `expect`, with each carriage return written as `\r`, and each of U+2028 to U+202E as
/// `\u{…}`: what it evaluates to, or the problems of its `--arg`s; and that every prefix of each
/// file it holds evaluates or gives problems that read.
#[track_caller]
fn check_within(bounds: Bounds, fixture: &str, expect: &Expect) {
    expect.assert_eq(&shown(bounds, fixture));
}

/// What `check_within` checks of `fixture`, within `bounds`.
#[track_caller]
fn shown(bounds: Bounds, fixture: &str) -> String {
    let files = files();
    let shown = on_stack(|| {
        let (args, case) = Case::parse(fixture, &files, bounds);
        let args = match Args::parse(args) {
            Ok(args) => args,
            Err(problems) => return problems.join("\n"),
        };
        let (Ok(shown) | Err(shown)) = case.evaluate(&args);
        // Only that no prefix panics: how many evaluate is what a fixture's file shows.
        for (path, _) in &case.held {
            case.prefixes(path, &args, 1);
        }
        shown
    });
    // A raw string literal can't hold a carriage return, and rustc refuses a literal that holds
    // a character that changes the direction of text.
    let shown: String = (shown.chars())
        .map(|char| match char {
            '\r' => String::from("\\r"),
            '\u{2028}'..='\u{202e}' => format!("\\u{{{:x}}}", u32::from(char)),
            _ => String::from(char),
        })
        .collect();
    shown
}

/// What a stand-in build gives for an image: its number, from 1, in the order built, and an ID
/// of 64 digits, `b`s and then the number.
struct Built {
    number: usize,
    image: ir::Image,
}

impl AsRef<ir::Image> for Built {
    fn as_ref(&self) -> &ir::Image {
        &self.image
    }
}

/// The workflow that the entry at `path` gives, given `args`, within `bounds`, through each
/// image's build, which a line of `built` describes, as does each step of the builds, by the
/// images it builds at once: a stand-in check refuses each recipe of the package `bad/refused`,
/// naming its named contexts, and each of its mounts, and a stand-in build, of [`JOBS`] images at
/// once, the oldest ending at each step, fails each of `bad/broken`'s.
fn run(
    path: &str,
    loader: &mut impl Loader,
    args: &Args,
    bounds: Bounds,
    built: &mut Vec<String>,
) -> Result<Workflow, Problems> {
    // A library's file by its label, as `Case::parse` names it.
    let entry = if path.starts_with('@') {
        Label::parse(path)
    } else {
        Label::parse(&format!("//{path}"))
    };
    let plan = lang::evaluate(&entry.unwrap(), loader, args, bounds)?;
    plan.check(|recipe, contexts| {
        let Recipe {
            context,
            recipe,
            mounts,
            ..
        } = recipe;
        if context.path() != "bad/refused" {
            return Vec::new();
        }
        let mounts = mounts
            .keys()
            .map(|at| format!("the stand-in check refuses the mount at `{at}`"));
        let given: Vec<String> = (contexts.iter())
            .map(|name| format!(", given {name}"))
            .collect();
        [format!(
            "the stand-in check refuses `{recipe}`{}",
            given.concat()
        )]
        .into_iter()
        .chain(mounts)
        .collect()
    })?;
    let mut running: VecDeque<(usize, usize, Result<Built, String>)> = VecDeque::new();
    let mut started = 0;
    plan.build(JOBS, |ready: Vec<(usize, Declared<'_, Built>)>| {
        for (index, declared) in ready {
            started += 1;
            running.push_back((index, started, stand_in(declared, started, built)));
        }
        let numbers: Vec<String> = (running.iter())
            .map(|(_, number, _)| number.to_string())
            .collect();
        built.push(format!("building {}", numbers.join(" and ")));
        let ended = running
            .pop_front()
            .map(|(index, _, result)| (index, result));
        Vec::from_iter(ended)
    })
}

/// How many images the stand-in build of [`run`] builds at once.
const JOBS: NonZeroUsize = NonZeroUsize::new(2).unwrap();

/// What the stand-in build of [`run`] gives for `declared`, the `number`th image started, with a
/// line of `built` that describes it.
fn stand_in(
    declared: Declared<'_, Built>,
    number: usize,
    built: &mut Vec<String>,
) -> Result<Built, String> {
    let Declared {
        recipe:
            Recipe {
                context,
                recipe,
                mounts,
                target,
                args,
                repository,
            },
        contexts,
    } = declared;
    if context.path() == "bad/broken" {
        return Err(format!("the stand-in build fails `{recipe}`"));
    }
    let mounts = (mounts.iter()).map(|(at, dir)| format!(", with {dir} at {at}"));
    let target = target.iter().map(|target| format!(", its stage {target}"));
    let args = (args.iter()).map(|(name, value)| format!(", with {name}={value:?}"));
    let contexts =
        (contexts.iter()).map(|(name, built)| format!(", with image {} as {name}", built.number));
    let given: String = mounts.chain(target).chain(args).chain(contexts).collect();
    let line = format!("{context}'s {recipe}{given}, in {repository}");
    built.push(format!("image {number}: {line}"));
    let image = ir::Image::try_from(format!("sha256:{number:b>64}")).unwrap();
    Ok(Built { number, image })
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshots"
)]
fn refusals() {
    check(
        r#"
#- //bad/names/task.star
load("//lib/parts.star", "AGENT", "_DRAFT")

ROLES = [AGENT]
ROLES = [AGENT, FINAL_TARGET]

load("//lib/parts.star", "FINAL")

workflow(name = "neutral/names", roles = ROLES, final = FINLA, deadline_seconds = 3600)
"#,
        &expect![[r#"
            bad/names/task.star:1:35: `_DRAFT` is private to `//lib/parts.star`: load a name that doesn't start with `_`
                1 | load("//lib/parts.star", "AGENT", "_DRAFT")
                  |                                   ^^^^^^^^
            bad/names/task.star:4:1: `ROLES` is bound already: bind each global once
                4 | ROLES = [AGENT, FINAL_TARGET]
                  | ^^^^^
                it is first bound at bad/names/task.star:3:1
            bad/names/task.star:4:17: `FINAL_TARGET` is not defined: bind it in this file, or load it
                4 | ROLES = [AGENT, FINAL_TARGET]
                  |                 ^^^^^^^^^^^^
            bad/names/task.star:6:1: a `load` after other statements is not in Pumpkin's Starlark: put every `load` at the top of the file
                6 | load("//lib/parts.star", "FINAL")
                  | ^^^^
                the first other statement is at bad/names/task.star:3:1
            bad/names/task.star:8:57: `FINLA` is not defined: bind it in this file, or load it
                8 | workflow(name = "neutral/names", roles = ROLES, final = FINLA, deadline_seconds = 3600)
                  |                                                         ^^^^^"#]],
    );
    check(
        r#"
#- //bad/cycle/task.star
load("//bad/cycle/a.star", "A")
#- //bad/cycle/a.star
load("//bad/cycle/b.star", "B")

A = B
#- //bad/cycle/b.star
load("//bad/cycle/a.star", "A")

B = A
"#,
        &expect![[r#"
            bad/cycle/b.star:1:6: `//bad/cycle/b.star` loads `//bad/cycle/a.star`, which is loading already: break the cycle
                1 | load("//bad/cycle/a.star", "A")
                  |      ^^^^^^^^^^^^^^^^^^^^
                `//bad/cycle/a.star` loads `//bad/cycle/b.star` at bad/cycle/a.star:1:6"#]],
    );
    check(
        r#"
#- //bad/missing/task.star
load("//lib/absent.star", "ABSENT")
"#,
        &expect![[r#"
            bad/missing/task.star:1:6: loading `//lib/absent.star`: no fixture is at `lib/absent.star`: name a file in the fixtures
                1 | load("//lib/absent.star", "ABSENT")
                  |      ^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"
#- //bad/syntax/task.star
workflow(
    name = "neutral/syntax"
    roles = [],
)
"#,
        &expect![[r#"
            bad/syntax/task.star:3:5: unexpected `roles`: expected `,` or `)`
                3 |     roles = [],
                  |     ^^^^^"#]],
    );
    check(
        "
#- //bad/deep/task.star
# Lists nested 129 deep, one more than the language takes.
DEEP = [
    [[[[[[[[[[[[[[[[
    [[[[[[[[[[[[[[[[
    [[[[[[[[[[[[[[[[
    [[[[[[[[[[[[[[[[
    [[[[[[[[[[[[[[[[
    [[[[[[[[[[[[[[[[
    [[[[[[[[[[[[[[[[
    [[[[[[[[[[[[[[[[
    ]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]
    ]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]
",
        &expect![[r#"
            bad/deep/task.star:10:20: this nests 129 deep: nest expressions and blocks at most 128 deep, binding inner parts to names
                10 |     [[[[[[[[[[[[[[[[
                   |                    ^"#]],
    );
    // A type nests as brackets do, so that reading it, and checking a value against it, keeps to
    // the stack.
    check(
        &format!(
            "#- //text.star\nload(\"//lib.star\", \"X\")\n#- //lib.star\ndef f(x: {}int{}):\n    pass",
            "list[\n".repeat(128),
            "]".repeat(128)
        ),
        &expect![[r#"
            lib.star:128:5: this nests 129 deep: nest a type at most 128 deep
                128 | list[
                    |     ^"#]],
    );
    check(
        r#"
#- //bad/nested/task.star
# A bracket and a `-`, which nest no deeper once closed, then lists nested 128 deep, as deep as
# a value may nest, then a dict around them, through a name.
NEGATED = -(-1)

DEEP = [[[[[[[[[[[[[[[[
    [[[[[[[[[[[[[[[[
    [[[[[[[[[[[[[[[[
    [[[[[[[[[[[[[[[[
    [[[[[[[[[[[[[[[[
    [[[[[[[[[[[[[[[[
    [[[[[[[[[[[[[[[[
    [[[[[[[[[[[[[[[[
    ]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]
    ]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]

DEEPER = {"deep": DEEP}
"#,
        &expect![[r#"
            bad/nested/task.star:16:10: this makes a value nested 129 deep: nest lists, tuples, dicts and functions' defaults at most 128 deep
                16 | DEEPER = {"deep": DEEP}
                   |          ^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"
#- //bad/status/task.star
load("//lib/parts.star", "AGENT", "FINAL_TARGET", "FINAL")

workflow(
    name = "neutral/status",
    roles = [AGENT, FINAL_TARGET | {"status": "ready"}],
    final = FINAL,
    deadline_seconds = 3600,
)
"#,
        &expect![[r#"
            bad/status/task.star:5:37: `status` is no field of an element of `roles`: expected one of `name`, `image`, `user`, `needs`, `runtime`
                5 |     roles = [AGENT, FINAL_TARGET | {"status": "ready"}],
                  |                                     ^^^^^^^^
                the IR's /roles/1"#]],
    );
    check(
        r#"
#- //bad/zero/task.star
load("//lib/parts.star", "AGENT", "FINAL_TARGET", "FINAL")

CPU_MILLIS = 0

workflow(
    name = "neutral/zero",
    roles = [AGENT, FINAL_TARGET | {"needs": {"cpu_millis": CPU_MILLIS, "memory_bytes": 512 * MiB}}],
    final = FINAL,
    deadline_seconds = 3600,
)
"#,
        &expect![[r#"
            bad/zero/task.star:3:14: `cpu_millis` can't be `0`: expected a positive int
                3 | CPU_MILLIS = 0
                  |              ^
                the IR's /roles/1/needs/cpu_millis"#]],
    );
    check(
        r#"
#- //bad/payload/task.star
load("//lib/parts.star", "AGENT", "FINAL_TARGET", "FINAL")
load("//lib/submit.star", "submit")

# Over the op payload limit, 64 MiB, which holds every payload.
MAX_BYTES = 65 * MiB

# Each part that takes it is refused, at the place it is made.
GRANTS = [{"name": "final", "now": {"body": submit, "payload_max_bytes": MAX_BYTES}}]

workflow(
    name = "neutral/payload",
    roles = [AGENT | {"runtime": {"agent": AGENT["runtime"]["agent"] | {"grants": GRANTS}}}, FINAL_TARGET],
    final = FINAL | {"payload_max_bytes": MAX_BYTES},
    deadline_seconds = 3600,
)
"#,
        &expect![[r#"
            bad/payload/task.star:5:13: `final`'s `payload_max_bytes` is 68157440, over the op payload limit of 67108864: give at most that
                5 | MAX_BYTES = 65 * MiB
                  |             ^^^^^^^^
                the IR's /roles/0/runtime/agent/grants/0/now/payload_max_bytes
            bad/payload/task.star:5:13: the final call's `payload_max_bytes` is 68157440, over the op payload limit of 67108864: give at most that
                5 | MAX_BYTES = 65 * MiB
                  |             ^^^^^^^^
                the IR's /final/payload_max_bytes"#]],
    );
    check(
        r#"
#- //bad/keyword/task.star
load("//lib/neutral.star", "neutral_task")

neutral_task(
    task = "tasks/keyword",
    agent = {"cpu_millis": 4000, "memory_bytes": 8 * GiB},
    metric = ["solved"],
)
"#,
        &expect![[r#"
            bad/keyword/task.star:6:5: `metric` is no parameter of `neutral_task`: expected one of `task`, `agent`, `metrics`, `calls`, `judge`, `env`, `models`
                6 |     metric = ["solved"],
                  |     ^^^^^^"#]],
    );
    check(
        r#"
#- //bad/operand/task.star
load("//lib/neutral.star", "neutral_task")

neutral_task(
    task = "tasks/operand",
    agent = {"cpu_millis": 4000, "memory_bytes": 8 * GiB},
    metrics = ["solved"],
    env = {"SEED": 7},
)
"#,
        &expect![[r#"
            lib/neutral.star:16:23: `+` can't take a string and an int: give it two numbers, two strings, two lists or two tuples
                16 |     return [key + "=" + value for key, value in env.items()] + [
                   |                       ^
                the left operand, `"SEED="`, is made at lib/neutral.star:16:13
                the right operand, `7`, is made at bad/operand/task.star:7:20
                in `_env()`, called at lib/neutral.star:28:63
                in `_trainer()`, called at lib/neutral.star:111:13
                in `neutral_task()`, called at bad/operand/task.star:3:1"#]],
    );
    check(
        r#"
#- //bad/recursion/task.star
load("//bad/recursion/b.star", "COUNT")
#- //bad/recursion/a.star
"""A library whose function calls the one it is given."""

def apply(function, value):
    return function(value)
#- //bad/recursion/b.star
"""A library whose function calls itself again, through another library's."""

load("//bad/recursion/a.star", "apply")

def countdown(n):
    if n == 0:
        return 0
    return apply(countdown, n - 1)

COUNT = countdown(3)
"#,
        &expect![[r#"
            bad/recursion/a.star:4:12: this calls `countdown` while it runs, and a Starlark function never calls itself, even through others: compute the value without calling `countdown` again
                4 |     return function(value)
                  |            ^^^^^^^^^^^^^^^
                in `apply()`, called at bad/recursion/b.star:8:12
                in `countdown()`, called at bad/recursion/b.star:10:9"#]],
    );
    check(
        r#"
#- //bad/nesting/task.star
load("//bad/nesting/lib.star", "DEEPEST")
#- //bad/nesting/lib.star
"""Functions whose bodies nest in parentheses, `inner`'s 65 deep and that of `outer`, which
calls it, 62 deep: `outer()` evaluates 128 deep, as deep as the language evaluates, counting the
calls, and `[outer()]` 129."""

def inner():
    return 1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (
        1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (
        1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (
        1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (
        1))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))

def outer():
    return 1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (
        1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (
        1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (
        1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (inner()
        ))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))

DEEPEST = outer()

TOO_DEEP = [outer()]
"#,
        &expect![[r#"
            bad/nesting/lib.star:16:69: this call nests `inner`'s body 129 deep, counting the calls it is in: nest expressions, blocks and calls at most 128 deep
                16 |         1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (1 + (inner()
                   |                                                                     ^^^^^^^
                in `outer()`, called at bad/nesting/lib.star:21:13"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshots"
)]
fn snippets() {
    // A tuple without parentheses can't end with a comma.
    check(
        r#"ARGV = "/a","#,
        &expect![[r#"
            text.star:1:12: a tuple without parentheses can't end with `,`: drop the `,`, or put the tuple in parentheses
                1 | ARGV = "/a",
                  |            ^"#]],
    );
    check(
        "X, = [1]",
        &expect![[r#"
            text.star:1:2: a tuple without parentheses can't end with `,`: drop the `,`, or put the tuple in parentheses
                1 | X, = [1]
                  |  ^"#]],
    );
    check(
        "workflow(),",
        &expect![[r#"
            text.star:1:11: a tuple without parentheses can't end with `,`: drop the `,`, or put the tuple in parentheses
                1 | workflow(),
                  |           ^"#]],
    );
    // Statements, ints and line ends.
    check(
        "pass; X = 1; Y = 2;",
        &expect!["text.star: this file calls no `workflow()`: call it once, with the workflow"],
    );
    check(
        "X = 1 Y = 2",
        &expect![[r#"
            text.star:1:7: unexpected `Y`: expected the end of the line
                1 | X = 1 Y = 2
                  |       ^"#]],
    );
    check(
        "X = 007",
        &expect![[r#"
            text.star:1:5: `007` has a leading zero: write it without, or after `0o` if it is octal
                1 | X = 007
                  |     ^^^"#]],
    );
    check(
        "X = 1ifx",
        &expect![[r#"
        text.star:1:5: `1ifx` is no number: write digits of its base only, and a space between a number and a name
            1 | X = 1ifx
              |     ^^^^"#]],
    );
    check(
        "é = 1",
        &expect![[r#"
            text.star:1:1: a letter outside ASCII in a name, `é`, is not in Pumpkin's Starlark: write names in ASCII letters, digits and `_`
                1 | é = 1
                  | ^"#]],
    );
    check(
        "X = 1if True else 2",
        &expect![[r#"
            text.star:1:5: `1if`, a number and then `if`, is not in Pumpkin's Starlark: put a space between them
                1 | X = 1if True else 2
                  |     ^^^"#]],
    );
    check(
        "X = 0b101",
        &expect![[r#"
            text.star:1:5: `0b101` is binary, which Starlark's ints are not written in: write it in decimal, or after `0x` or `0o`
                1 | X = 0b101
                  |     ^^^^^"#]],
    );
    check(
        "X = 1\r\nY = \"a\\\r\nb\"\r\n",
        &expect!["text.star: this file calls no `workflow()`: call it once, with the workflow"],
    );
    check(
        "X = \"a\\\r\r\nb\"",
        &expect![[r#"
            text.star:1:8: a carriage return that ends no line is not in Pumpkin's Starlark: end lines with `\n` or `\r\n`
                1 | X = "a\\r
                  |        ^"#]],
    );
    check(
        r#"load("//lib/parts.star", X, "AGENT")"#,
        &expect![[r#"
            text.star:1:26: unexpected `X`: expected a name to load, as `"x"` or `y = "x"`
                1 | load("//lib/parts.star", X, "AGENT")
                  |                          ^"#]],
    );
    // Indentation, and tabs.
    check(
        "
X = 1
  Y = 2
",
        &expect![[r#"
            text.star:2:1: this line is indented, and no block holds it: start it at its line's start
                2 |   Y = 2
                  | ^^"#]],
    );
    check(
        "
X = 1
  \tY = 2
",
        &expect![[r#"
            text.star:2:3: a tab in indentation is not in Pumpkin's Starlark: indent with spaces only
                2 |   	Y = 2
                  |   ^"#]],
    );
    check(
        "
X = 1
\t
\t# a note
Y = 2
",
        &expect!["text.star: this file calls no `workflow()`: call it once, with the workflow"],
    );
    check(
        "X =\t1",
        &expect![[r#"
            text.star:1:4: a tab between tokens is not in Pumpkin's Starlark: separate them with spaces
                1 | X =	1
                  |    ^"#]],
    );
    // What the subset leaves out of the specification.
    check(
        "X += 1",
        &expect![[r#"
            text.star:1:3: `+=` is not in Pumpkin's Starlark: bind the result with `=`
                1 | X += 1
                  |   ^^"#]],
    );
    check(
        "X = 1 << 2",
        &expect![[r#"
            text.star:1:7: `<<`, a bitwise operator, is not in Pumpkin's Starlark: write the int it gives
                1 | X = 1 << 2
                  |       ^^"#]],
    );
    check(
        "x = /",
        &expect![[r#"
            text.star:1:5: unexpected `/`: expected an expression
                1 | x = /
                  |     ^"#]],
    );
    check(
        "def f(): def g(): pass",
        &expect![[r#"
            text.star:1:10: unexpected `def`: expected an expression
                1 | def f(): def g(): pass
                  |          ^^^"#]],
    );
    check(
        "x = 1 if 2 += 3",
        &expect![[r#"
            text.star:1:12: this conditional expression has no `else`: give the value when the condition is false, after `else`
                1 | x = 1 if 2 += 3
                  |            ^^"#]],
    );
    check(
        "x = 1; for y in []: pass",
        &expect![[r#"
            text.star:1:8: unexpected `for`: expected an expression
                1 | x = 1; for y in []: pass
                  |        ^^^"#]],
    );
    check(
        "X = 1 ~ 2",
        &expect![[r#"
            text.star:1:7: unexpected `~`: expected the end of the line
                1 | X = 1 ~ 2
                  |       ^"#]],
    );
    check(
        "X = (1,) < (2,)",
        &expect![[r#"
            text.star:1:10: `<` of two tuples is not in Pumpkin's Starlark: order two numbers or two strings
                1 | X = (1,) < (2,)
                  |          ^
                the left operand, a tuple, is made at text.star:1:5
                the right operand, a tuple, is made at text.star:1:12"#]],
    );
    check(
        "X = 1 if 2 << 3 else 4",
        &expect![[r#"
            text.star:1:12: `<<`, a bitwise operator, is not in Pumpkin's Starlark: write the int it gives
                1 | X = 1 if 2 << 3 else 4
                  |            ^^"#]],
    );
    check(
        "X = 1 | 2",
        &expect![[r#"
            text.star:1:7: `|` of two ints, a bitwise OR, is not in Pumpkin's Starlark: write the int it gives
                1 | X = 1 | 2
                  |       ^"#]],
    );
    check(
        "X = +1",
        &expect![[r#"
            text.star:1:5: unary `+` is not in Pumpkin's Starlark: write the number alone
                1 | X = +1
                  |     ^"#]],
    );
    check(
        r#"load("//lib/parts.star", "X" + "Y")"#,
        &expect![[r#"
            text.star:1:30: unexpected `+`: expected `,` or `)`
                1 | load("//lib/parts.star", "X" + "Y")
                  |                              ^"#]],
    );
    check(
        "X = 2 ** 3",
        &expect![[r#"
            text.star:1:7: `**` is no operator of Starlark's: multiply, or use `KiB`, `MiB` or `GiB`
                1 | X = 2 ** 3
                  |       ^^"#]],
    );
    check(
        "workflow(*X)",
        &expect![[r#"
            text.star:1:10: `*` before an argument, which passes a sequence's elements, is not in Pumpkin's Starlark: pass each argument by itself
                1 | workflow(*X)
                  |          ^"#]],
    );
    check(
        "workflow(**X, 1)",
        &expect![[r#"
        text.star:1:15: a positional argument follows `**`: put positional arguments first
            1 | workflow(**X, 1)
              |               ^"#]],
    );
    check(
        "workflow(**X, name = 1)",
        &expect![[r#"
        text.star:1:15: a keyword argument follows `**`: put `**` last
            1 | workflow(**X, name = 1)
              |               ^^^^"#]],
    );
    check(
        "workflow(**X, **X)",
        &expect![[r#"
        text.star:1:15: this is a second `**`: pass one dict, joining them with `|`
            1 | workflow(**X, **X)
              |               ^^"#]],
    );
    check(
        "X = lambda: 1",
        &expect![[r#"
            text.star:1:5: `lambda` is not in Pumpkin's Starlark: write a `def` at a library's top level, and pass its name
                1 | X = lambda: 1
                  |     ^^^^^^"#]],
    );
    check(
        "for X in []: pass",
        &expect![[r#"
            text.star:1:1: a `for` statement at a file's top level is not in Starlark: build the list with a comprehension, as `[f(x) for x in xs]`
                1 | for X in []: pass
                  | ^^^"#]],
    );
    check(
        "while True: pass",
        &expect![[r#"
            text.star:1:1: `while` is not in Starlark: build the list with a comprehension, as `[f(x) for x in xs]`
                1 | while True: pass
                  | ^^^^^"#]],
    );
    check(
        "break",
        &expect![[r#"
            text.star:1:1: `break` stands only in a loop's body, and no loop holds it: filter a comprehension with `if`
                1 | break
                  | ^^^^^"#]],
    );
    check(
        "X = {1, 2}",
        &expect![[r#"
            text.star:1:5: a set literal is not in Starlark: write a list, or give each key a value after `:`
                1 | X = {1, 2}
                  |     ^"#]],
    );
    check(
        "X: int = 1",
        &expect![[r#"
            text.star:1:2: an annotation of a variable is not in Pumpkin's Starlark: drop the `:` and the type
                1 | X: int = 1
                  |  ^"#]],
    );
    check(
        r#"X = "%s" % "a""#,
        &expect![[r#"
            text.star:1:10: `%` of a string, which formats it, is not in Pumpkin's Starlark: format it with `format`, or join strings with `+`
                1 | X = "%s" % "a"
                  |          ^"#]],
    );
    check(
        "X = [1] * 2",
        &expect![[r#"
            text.star:1:9: `*` of a sequence and an int, which would let an expression make a value of any size, is not in Pumpkin's Starlark: write the elements out, or join with `+`
                1 | X = [1] * 2
                  |         ^"#]],
    );
    check(
        "X = 1e400",
        &expect![[r#"
            text.star:1:5: `1e400` is too large for a float: write one of at most 1.7976931348623157e+308
                1 | X = 1e400
                  |     ^^^^^"#]],
    );
    check(
        "X = 1e+",
        &expect![[r#"
            text.star:1:5: `1e+` has no digits in its exponent: write some after its `e`, as `1e3`
                1 | X = 1e+
                  |     ^^^"#]],
    );
    check(
        "X = 1.5if True else 2",
        &expect![[r#"
        text.star:1:5: `1.5if`, a number and then `if`, is not in Pumpkin's Starlark: put a space between them
            1 | X = 1.5if True else 2
              |     ^^^^^"#]],
    );
    check(
        r#"X = f"a""#,
        &expect![[r#"
            text.star:1:5: `f` starts an f-string, which Starlark lacks: join strings with `+`
                1 | X = f"a"
                  |     ^"#]],
    );
    // Escapes that some of Starlark's implementations lack, though not in a raw string, where a
    // `\` stands for itself.
    check(
        r#"X = "\x41""#,
        &expect![[r#"
        text.star:1:6: `\x`, an escape that some Starlark implementations lack, is not in Pumpkin's Starlark: write the character itself, or an ASCII one in octal, as `\101` for `A`
            1 | X = "\x41"
              |      ^^"#]],
    );
    check(
        r#"fail(r"\x41\u00e9")"#,
        &expect![[r#"
        text.star:1:1: fail: \x41\u00e9
            1 | fail(r"\x41\u00e9")
              | ^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = "\200""#,
        &expect![[r#"
            text.star:1:6: `\200`, an octal escape over 127, which Starlark's implementations read differently, is not in Pumpkin's Starlark: write the character itself, or an ASCII one, up to `\177`
                1 | X = "\200"
                  |      ^^^^"#]],
    );
    check(
        r#"X = b"a""#,
        &expect![[r#"
            text.star:1:5: a bytes literal, which `b` starts, is not in Pumpkin's Starlark: write a string
                1 | X = b"a"
                  |     ^"#]],
    );
    check(
        "X = [1][0:1]",
        &expect![[r#"
            text.star:1:10: a slice is not in Pumpkin's Starlark: index the elements one by one
                1 | X = [1][0:1]
                  |          ^"#]],
    );
    check(
        "X = [1][:1]",
        &expect![[r#"
            text.star:1:9: a slice is not in Pumpkin's Starlark: index the elements one by one
                1 | X = [1][:1]
                  |         ^"#]],
    );
    check(
        "X = (x for x in [])",
        &expect![[r#"
            text.star:1:8: a generator is not in Starlark: build a list, as `[x for x in xs]`
                1 | X = (x for x in [])
                  |        ^^^"#]],
    );
    check(
        "X = {x for x in []}",
        &expect![[r#"
            text.star:1:5: a set literal is not in Starlark: write a list, or give each key a value after `:`
                1 | X = {x for x in []}
                  |     ^"#]],
    );
    check(
        r#"X = "a".split"#,
        &expect![[r#"
            text.star:1:9: a method named but not called is not in Pumpkin's Starlark: call it here, as `.split(…)`
                1 | X = "a".split
                  |         ^^^^^"#]],
    );
    check(
        "X = 1 < 2 < 3",
        &expect![[r#"
            text.star:1:11: comparisons don't chain in Starlark: join them with `and`
                1 | X = 1 < 2 < 3
                  |           ^"#]],
    );
    check(
        "X = 1 == not 2",
        &expect![[r#"
            text.star:1:10: unexpected `not`: expected an expression
                1 | X = 1 == not 2
                  |          ^^^"#]],
    );
    check(
        "X = 1 + 1 == 2 == True",
        &expect![[r#"
            text.star:1:16: comparisons don't chain in Starlark: join them with `and`
                1 | X = 1 + 1 == 2 == True
                  |                ^^"#]],
    );
    check(
        "X = 1 not not [1]",
        &expect![[r#"
            text.star:1:7: unexpected `not`: expected the end of the line
                1 | X = 1 not not [1]
                  |       ^^^"#]],
    );
    check(
        "X = len(x for x in [])",
        &expect![[r#"
            text.star:1:11: unexpected `for`: expected `,` or `)`
                1 | X = len(x for x in [])
                  |           ^^^"#]],
    );
    check(
        "X = [x for x, in [[1]]]",
        &expect![[r#"
            text.star:1:13: a tuple without parentheses can't end with `,`: drop the `,`, or put the tuple in parentheses
                1 | X = [x for x, in [[1]]]
                  |             ^"#]],
    );
    check(
        "X = [1 if True for x in []]",
        &expect![[r#"
            text.star:1:16: this conditional expression has no `else`: give the value when the condition is false, after `else`
                1 | X = [1 if True for x in []]
                  |                ^^^"#]],
    );
    check(
        "X = [x for x [1]]",
        &expect![[r#"
            text.star:1:17: unexpected `]`: expected `in`, then what to iterate
                1 | X = [x for x [1]]
                  |                 ^"#]],
    );
    check(
        "X = [1][0] = 2",
        &expect![[r#"
            text.star:1:12: unexpected `=`: expected the end of the line
                1 | X = [1][0] = 2
                  |            ^"#]],
    );
    check(
        "
if True:
    X = 1",
        &expect![[r#"
            text.star:1:1: an `if` statement stands only in a function's body: choose a value with `a if c else b`, or move the statement into a `def`
                1 | if True:
                  | ^^"#]],
    );
    check(
        "return 1",
        &expect![[r#"
            text.star:1:1: `return` stands only in a function's body: move it into a `def`
                1 | return 1
                  | ^^^^^^"#]],
    );
    check(
        "
def f():
    pass",
        &expect![[r#"
            text.star:1:1: a function defined in an entry file is not in Pumpkin's Starlark: define it in a library, and load it
                1 | def f():
                  | ^^^"#]],
    );
    check(
        r#"print("a")"#,
        &expect![[r#"
            text.star:1:1: `print`, a builtin of Starlark's whose output nothing reads, is not in Pumpkin's Starlark: show a value with `fail`
                1 | print("a")
                  | ^^^^^"#]],
    );
    check(
        "X = range(3)",
        &expect![[r#"
            text.star:1:5: `range`, a builtin of Starlark's, is not in Pumpkin's Starlark: write the value out, or build it with a comprehension
                1 | X = range(3)
                  |     ^^^^^"#]],
    );
    check(
        "X = type(1)",
        &expect![[r#"
            text.star:1:5: `type`, a builtin of Starlark's, is not in Pumpkin's Starlark: compute the value with `bool`, `float`, `int`, `len`, `sorted` and `str`
                1 | X = type(1)
                  |     ^^^^"#]],
    );
    // Errors while a file runs.
    check(
        "X = 9223372036854775807 + 1",
        &expect![[r#"
            text.star:1:25: `9223372036854775807 + 1`, which overflows 64 bits, is not in Pumpkin's Starlark: keep ints from -2^63 to 2^63 - 1, since ints are 64-bit
                1 | X = 9223372036854775807 + 1
                  |                         ^"#]],
    );
    check(
        "X = -9223372036854775807 - 2",
        &expect![[r#"
            text.star:1:26: `-9223372036854775807 - 2`, which overflows 64 bits, is not in Pumpkin's Starlark: keep ints from -2^63 to 2^63 - 1, since ints are 64-bit
                1 | X = -9223372036854775807 - 2
                  |                          ^"#]],
    );
    check(
        "X = 4611686018427387904 * 2",
        &expect![[r#"
            text.star:1:25: `4611686018427387904 * 2`, which overflows 64 bits, is not in Pumpkin's Starlark: keep ints from -2^63 to 2^63 - 1, since ints are 64-bit
                1 | X = 4611686018427387904 * 2
                  |                         ^"#]],
    );
    check(
        "X = (-9223372036854775807 - 1) // -1",
        &expect![[r#"
            text.star:1:32: `-9223372036854775808 // -1`, which overflows 64 bits, is not in Pumpkin's Starlark: keep ints from -2^63 to 2^63 - 1, since ints are 64-bit
                1 | X = (-9223372036854775807 - 1) // -1
                  |                                ^^"#]],
    );
    check(
        "X = -(-9223372036854775807 - 1)",
        &expect![[r#"
            text.star:1:5: `-(-9223372036854775808)`, which overflows 64 bits, is not in Pumpkin's Starlark: keep ints from -2^63 to 2^63 - 1, since ints are 64-bit
                1 | X = -(-9223372036854775807 - 1)
                  |     ^"#]],
    );
    check(
        "X = 1 % 0",
        &expect![[r#"
            text.star:1:7: `1 % 0` divides by zero: divide by another int
                1 | X = 1 % 0
                  |       ^"#]],
    );
    check(
        "
X = 0 and 1 // 0
Y = 1 or 1 // 0",
        &expect!["text.star: this file calls no `workflow()`: call it once, with the workflow"],
    );
    check(
        "X = 1 // 0",
        &expect![[r#"
            text.star:1:7: `1 // 0` divides by zero: divide by another int
                1 | X = 1 // 0
                  |       ^^"#]],
    );
    check(
        "X = 1.5 / 0",
        &expect![[r#"
            text.star:1:9: `1.5 / 0` divides by zero: divide by another number
                1 | X = 1.5 / 0
                  |         ^"#]],
    );
    check(
        "X = 7 // 0.0",
        &expect![[r#"
            text.star:1:7: `7 // 0.0` divides by zero: divide by another number
                1 | X = 7 // 0.0
                  |       ^^"#]],
    );
    check(
        "X, Y = [1, 2, 3]",
        &expect![[r#"
            text.star:1:1: a list of 3 values can't be unpacked into 2 names: give a list or a tuple of 2
                1 | X, Y = [1, 2, 3]
                  | ^^^^
                the value, a list, is made at text.star:1:8"#]],
    );
    check(
        "X, Y = 1",
        &expect![[r#"
            text.star:1:1: an int can't be unpacked into names: give a list or a tuple of 2
                1 | X, Y = 1
                  | ^^^^
                the value, `1`, is made at text.star:1:8"#]],
    );
    check(
        "X = {[1]: 2}",
        &expect![[r#"
            text.star:1:6: a list can't be a dict's key: key a dict with strings or numbers
                1 | X = {[1]: 2}
                  |      ^^^"#]],
    );
    check(
        "X = {None: 1}",
        &expect![[r#"
            text.star:1:6: `None` as a dict's key is not in Pumpkin's Starlark: key a dict with strings or numbers
                1 | X = {None: 1}
                  |      ^^^^"#]],
    );
    check(
        "X = {(1, 2): 1}",
        &expect![[r#"
            text.star:1:6: a tuple as a dict's key is not in Pumpkin's Starlark: key a dict with strings or numbers
                1 | X = {(1, 2): 1}
                  |      ^^^^^^"#]],
    );
    check(
        "X, Y = {1: 2, 3: 4}",
        &expect![[r#"
        text.star:1:1: unpacking a dict is not in Pumpkin's Starlark: unpack the list of its `keys()`
            1 | X, Y = {1: 2, 3: 4}
              | ^^^^
            the value, a dict, is made at text.star:1:8"#]],
    );
    check(
        "X = [1][0, 1]",
        &expect![[r#"
            text.star:1:10: an index of several values is not in Pumpkin's Starlark: index with one value
                1 | X = [1][0, 1]
                  |          ^"#]],
    );
    check(
        "X = {[x]: x for x in [1]}",
        &expect![[r#"
            text.star:1:6: a list can't be a dict's key: key a dict with strings or numbers
                1 | X = {[x]: x for x in [1]}
                  |      ^^^"#]],
    );
    check(
        r#"X = "N=" + 3"#,
        &expect![[r#"
            text.star:1:10: `+` can't take a string and an int: give it two numbers, two strings, two lists or two tuples
                1 | X = "N=" + 3
                  |          ^
                the left operand, `"N="`, is made at text.star:1:5
                the right operand, `3`, is made at text.star:1:12"#]],
    );
    check(
        r#"X = -"a""#,
        &expect![[r#"
            text.star:1:5: `-` can't take a string: give it a number
                1 | X = -"a"
                  |     ^
                the operand, `"a"`, is made at text.star:1:6"#]],
    );
    check(
        r#"X = 1 < "a""#,
        &expect![[r#"
            text.star:1:7: `<` can't take an int and a string: give it two numbers or two strings
                1 | X = 1 < "a"
                  |       ^
                the left operand, `1`, is made at text.star:1:5
                the right operand, `"a"`, is made at text.star:1:9"#]],
    );
    check(
        "X = True < False",
        &expect![[r#"
            text.star:1:10: `<` of two bools is not in Pumpkin's Starlark: order two numbers or two strings
                1 | X = True < False
                  |          ^
                the left operand, `True`, is made at text.star:1:5
                the right operand, `False`, is made at text.star:1:12"#]],
    );
    check(
        "X = [1] < [2]",
        &expect![[r#"
            text.star:1:9: `<` of two lists is not in Pumpkin's Starlark: order two numbers or two strings
                1 | X = [1] < [2]
                  |         ^
                the left operand, a list, is made at text.star:1:5
                the right operand, a list, is made at text.star:1:11"#]],
    );
    check(
        "X = {} | []",
        &expect![[r#"
            text.star:1:8: `|` can't take a dict and a list: give it two dicts
                1 | X = {} | []
                  |        ^
                the left operand, a dict, is made at text.star:1:5
                the right operand, a list, is made at text.star:1:10"#]],
    );
    check(
        r#"X = 1 in "a""#,
        &expect![[r#"
            text.star:1:7: `in` can't find an int in a string: look for a string in it
                1 | X = 1 in "a"
                  |       ^^
                the left operand, `1`, is made at text.star:1:5"#]],
    );
    check(
        "X = [1] in {}",
        &expect![[r#"
            text.star:1:9: a list can't be a dict's key: key a dict with strings or numbers
                1 | X = [1] in {}
                  |         ^^
                the left operand, a list, is made at text.star:1:5"#]],
    );
    check(
        "X = 1 in 2",
        &expect![[r#"
            text.star:1:7: `in` can't look in an int: give it a list, a tuple, a dict or a string to look in
                1 | X = 1 in 2
                  |       ^^
                the right operand, `2`, is made at text.star:1:10"#]],
    );
    check(
        "X = [1][1]",
        &expect![[r#"
            text.star:1:9: `1` indexes no element of a list of 1: index from -1 to 0
                1 | X = [1][1]
                  |         ^
                the indexed value, a list, is made at text.star:1:5"#]],
    );
    check(
        "X = [][0]",
        &expect![[r#"
            text.star:1:8: `0` indexes no element of a list that is empty: index one that has some
                1 | X = [][0]
                  |        ^
                the indexed value, a list, is made at text.star:1:5"#]],
    );
    check(
        r#"X = {"a": 1}["b"]"#,
        &expect![[r#"
            text.star:1:14: `"b"` is no key of this dict: index it with one of its keys, or give a default with `get`
                1 | X = {"a": 1}["b"]
                  |              ^^^
                the indexed value, a dict, is made at text.star:1:5"#]],
    );
    check(
        r#"X = [1]["a"]"#,
        &expect![[r#"
            text.star:1:9: a string can't index a list: index it with an int
                1 | X = [1]["a"]
                  |         ^^^"#]],
    );
    check(
        "X = [1][0.0]",
        &expect![[r#"
            text.star:1:9: a float can't index a list: index it with an int
                1 | X = [1][0.0]
                  |         ^^^"#]],
    );
    check(
        r#"X = {"a": 1}[[1]]"#,
        &expect![[r#"
            text.star:1:14: a list can't be a dict's key: key a dict with strings or numbers
                1 | X = {"a": 1}[[1]]
                  |              ^^^"#]],
    );
    check(
        r#"X = "ab"[0]"#,
        &expect![[r#"
            text.star:1:10: indexing a string, whose elements Starlark's implementations count differently, is not in Pumpkin's Starlark: split it, or test it with `startswith`
                1 | X = "ab"[0]
                  |          ^
                the indexed value, `"ab"`, is made at text.star:1:5"#]],
    );
    check(
        "X = 1[0]",
        &expect![[r#"
            text.star:1:7: an int can't be indexed: index a list, a tuple or a dict
                1 | X = 1[0]
                  |       ^
                the indexed value, `1`, is made at text.star:1:5"#]],
    );
    check(
        r#"X = [c for c in "ab"]"#,
        &expect![[r#"
            text.star:1:12: a string can't be iterated over: split it into a list
                1 | X = [c for c in "ab"]
                  |            ^
                what is iterated, `"ab"`, is made at text.star:1:17"#]],
    );
    check(
        "X = [c for c in 1]",
        &expect![[r#"
            text.star:1:12: an int can't be iterated over: give a list, a tuple or a dict
                1 | X = [c for c in 1]
                  |            ^
                what is iterated, `1`, is made at text.star:1:17"#]],
    );
    check(
        "
X = 1
Y = X()",
        &expect![[r#"
            text.star:2:5: an int can't be called: call a function
                2 | Y = X()
                  |     ^
                the callee, `1`, is made at text.star:1:5"#]],
    );
    check(
        r#"X = len("ab")"#,
        &expect![[r#"
            text.star:1:9: `len` of a string, whose elements Starlark's implementations count differently, is not in Pumpkin's Starlark: give it a list, a tuple or a dict
                1 | X = len("ab")
                  |         ^^^^"#]],
    );
    check(
        "X = len(1)",
        &expect![[r#"
            text.star:1:9: `len` can't take an int: give it a list, a tuple or a dict
                1 | X = len(1)
                  |         ^"#]],
    );
    check(
        "X = len()",
        &expect![[r#"
            text.star:1:5: `len` takes 1 argument, and is given 0: give it 1 argument
                1 | X = len()
                  |     ^^^^^"#]],
    );
    check(
        "X = len(x = [])",
        &expect![[r#"
            text.star:1:9: `len` takes no keyword argument `x`: give it 1 argument, by position
                1 | X = len(x = [])
                  |         ^"#]],
    );
    check(
        "X = str([1])",
        &expect![[r#"
            text.star:1:9: `str` of a list is not in Pumpkin's Starlark: give it a number or a string
                1 | X = str([1])
                  |         ^^^"#]],
    );
    check(
        r#"X = int("0x10")"#,
        &expect![[r#"
            text.star:1:9: `int` can't read `"0x10"`: give it decimal digits, after `-` or `+` or neither
                1 | X = int("0x10")
                  |         ^^^^^^"#]],
    );
    check(
        r#"X = int("9223372036854775808")"#,
        &expect![[r#"
            text.star:1:9: `int` of `"9223372036854775808"`, outside -2^63 to 2^63 - 1, is not in Pumpkin's Starlark: give it a smaller int, since ints are 64-bit
                1 | X = int("9223372036854775808")
                  |         ^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        "X = int([])",
        &expect![[r#"
            text.star:1:9: `int` can't take a list: give it a number, a bool or a string of digits
                1 | X = int([])
                  |         ^^"#]],
    );
    check(
        "X = float([])",
        &expect![[r#"
            text.star:1:11: `float` can't take a list: give it a number, a bool or a string
                1 | X = float([])
                  |           ^^"#]],
    );
    check(
        r#"X = float("abc")"#,
        &expect![[r#"
            text.star:1:11: `float` can't read `"abc"`: give it a float's or an int's digits, or `inf` or `nan`, after `-` or `+` or neither
                1 | X = float("abc")
                  |           ^^^^^"#]],
    );
    check(
        r#"X = float("1e400")"#,
        &expect![[r#"
            text.star:1:11: `float` of `"1e400"` is too large for a float: give it at most 1.7976931348623157e+308
                1 | X = float("1e400")
                  |           ^^^^^^^"#]],
    );
    check(
        r#"X = int(float("nan"))"#,
        &expect![[r#"
            text.star:1:9: `int` can't take `nan`, which is not finite: give it a finite float
                1 | X = int(float("nan"))
                  |         ^^^^^^^^^^^^"#]],
    );
    check(
        "X = int(1e30)",
        &expect![[r#"
            text.star:1:9: `int` of `1e+30`, outside -2^63 to 2^63 - 1, is not in Pumpkin's Starlark: give it a smaller float, since ints are 64-bit
                1 | X = int(1e30)
                  |         ^^^^"#]],
    );
    check(
        r#"X = sorted([1, "a"])"#,
        &expect![[r#"
            text.star:1:12: `sorted` of a list of mixed or other values than numbers or strings is not in Pumpkin's Starlark: give it only numbers, or only strings
                1 | X = sorted([1, "a"])
                  |            ^^^^^^^^"#]],
    );
    check(
        "X = sorted([True])",
        &expect![[r#"
            text.star:1:12: `sorted` of a list of mixed or other values than numbers or strings is not in Pumpkin's Starlark: give it only numbers, or only strings
                1 | X = sorted([True])
                  |            ^^^^^^"#]],
    );
    check(
        "X = sorted(1)",
        &expect![[r#"
            text.star:1:12: `sorted` can't take an int: give it a list, a tuple or a dict
                1 | X = sorted(1)
                  |            ^"#]],
    );
    check(
        "fail([1])",
        &expect![[r#"
            text.star:1:6: `fail` of a list is not in Pumpkin's Starlark: give it a number or a string
                1 | fail([1])
                  |      ^^^"#]],
    );
    check(
        r#"X = "a".upper()"#,
        &expect![[r#"
            text.star:1:9: `upper` of a string is not in Pumpkin's Starlark: call one of `endswith`, `format`, `join`, `replace`, `split`, `startswith`
                1 | X = "a".upper()
                  |         ^^^^^
                the receiver, `"a"`, is made at text.star:1:5"#]],
    );
    check(
        "X = [1].index(1)",
        &expect![[r#"
            text.star:1:9: `index` of a list is not in Pumpkin's Starlark: give it to a function, as `len`
                1 | X = [1].index(1)
                  |         ^^^^^
                the receiver, a list, is made at text.star:1:5"#]],
    );
    check(
        "
X = [1]
Y = X.append(2)",
        &expect![[r#"
            text.star:2:7: `append`, which would change the list, is not in Pumpkin's Starlark: make a new list with `+`, as values never change
                2 | Y = X.append(2)
                  |       ^^^^^^"#]],
    );
    check(
        r#"
load("//lib/parts.star", "FINAL")
X = FINAL.update({})"#,
        &expect![[r#"
            text.star:2:11: `update`, which would change the dict, is not in Pumpkin's Starlark: make a new dict with `|`, as values never change
                2 | X = FINAL.update({})
                  |           ^^^^^^"#]],
    );
    check(
        r#"X = "a".split("")"#,
        &expect![[r#"
            text.star:1:15: `split` can't split at an empty separator: give it one
                1 | X = "a".split("")
                  |               ^^"#]],
    );
    check(
        r#"X = "a".split(1)"#,
        &expect![[r#"
            text.star:1:15: `split`'s argument can't be `1`: give a string
                1 | X = "a".split(1)
                  |               ^"#]],
    );
    check(
        r#"X = ",".join([1])"#,
        &expect![[r#"
            text.star:1:14: `join` can't join an int: give it a list or a tuple of strings
                1 | X = ",".join([1])
                  |              ^^^
                the element, `1`, is made at text.star:1:15"#]],
    );
    check(
        r#"X = ",".join("ab")"#,
        &expect![[r#"
            text.star:1:14: `join` can't take a string: give it a list or a tuple of strings
                1 | X = ",".join("ab")
                  |              ^^^^"#]],
    );
    check(
        r#"X = ",".join({"a": 1})"#,
        &expect![[r#"
        text.star:1:14: `join` of a dict is not in Pumpkin's Starlark: give it a list or a tuple of strings
            1 | X = ",".join({"a": 1})
              |              ^^^^^^^^"#]],
    );
    check(
        r#"X = "a b".split(None)"#,
        &expect![[r#"
        text.star:1:17: `split` of `None` is not in Pumpkin's Starlark: give it a string
            1 | X = "a b".split(None)
              |                 ^^^^"#]],
    );
    check(
        r#"X = "a b".split(" ", 1)"#,
        &expect![[r#"
        text.star:1:5: `split` with 2 arguments is not in Pumpkin's Starlark: give it 1 argument
            1 | X = "a b".split(" ", 1)
              |     ^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = "a".startswith(("a", "b"))"#,
        &expect![[r#"
        text.star:1:20: `startswith` of a tuple is not in Pumpkin's Starlark: give it a string
            1 | X = "a".startswith(("a", "b"))
              |                    ^^^^^^^^^^"#]],
    );
    check(
        r#"X = "é".replace("", "-")"#,
        &expect![[r#"
        text.star:1:17: `replace` of an empty string, which Starlark's implementations insert between different elements, is not in Pumpkin's Starlark: give it the text to replace
            1 | X = "é".replace("", "-")
              |                 ^^"#]],
    );
    check(
        r#"X = "a".replace("a", "b", 1)"#,
        &expect![[r#"
        text.star:1:5: `replace` with 3 arguments is not in Pumpkin's Starlark: give it 2 arguments
            1 | X = "a".replace("a", "b", 1)
              |     ^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        "X = sorted([2, 1], reverse = True)",
        &expect![[r#"
        text.star:1:20: `sorted`'s `reverse` is not in Pumpkin's Starlark: give `sorted` 1 argument, by position
            1 | X = sorted([2, 1], reverse = True)
              |                    ^^^^^^^"#]],
    );
    check(
        r#"X = int(x = "1")"#,
        &expect![[r#"
        text.star:1:9: `int`'s `x` is not in Pumpkin's Starlark: give `int` 1 argument, by position
            1 | X = int(x = "1")
              |         ^"#]],
    );
    check(
        "X = sorted(iterable = [1])",
        &expect![[r#"
        text.star:1:12: `sorted`'s `iterable` is not in Pumpkin's Starlark: give `sorted` 1 argument, by position
            1 | X = sorted(iterable = [1])
              |            ^^^^^^^^"#]],
    );
    check(
        "X = {(1, [2]): 1}",
        &expect![[r#"
            text.star:1:6: a tuple that holds an unhashable value can't be a dict's key: key a dict with strings or numbers
                1 | X = {(1, [2]): 1}
                  |      ^^^^^^^^"#]],
    );
    check(
        r#"X = "\q""#,
        &expect![[r#"
        text.star:1:6: `\q`, which some Starlark implementations read as itself, is not in Pumpkin's Starlark: write `\\` for a `\`
            1 | X = "\q"
              |      ^^"#]],
    );
    check(
        r#"X = "\8""#,
        &expect![[r#"
        text.star:1:6: `\8`, which some Starlark implementations read as itself, is not in Pumpkin's Starlark: write `\\` for a `\`
            1 | X = "\8"
              |      ^^"#]],
    );
    // Arguments that Bazel's Starlark takes, by name or by position.
    check(
        r#"fail(msg = "x")"#,
        &expect![[r#"
        text.star:1:6: `fail`'s `msg` is not in Pumpkin's Starlark: give `fail` 1 argument, by position
            1 | fail(msg = "x")
              |      ^^^"#]],
    );
    check(
        r#"fail("x", attr = "y")"#,
        &expect![[r#"
        text.star:1:11: `fail`'s `attr` is not in Pumpkin's Starlark: give `fail` 1 argument, by position
            1 | fail("x", attr = "y")
              |           ^^^^"#]],
    );
    check(
        r#"fail("x", stack_trace = False)"#,
        &expect![[r#"
        text.star:1:11: `fail`'s `stack_trace` is not in Pumpkin's Starlark: give `fail` 1 argument, by position
            1 | fail("x", stack_trace = False)
              |           ^^^^^^^^^^^"#]],
    );
    check(
        r#"X = "a,b".split(sep = ",")"#,
        &expect![[r#"
        text.star:1:17: `split`'s `sep` is not in Pumpkin's Starlark: give `split` 1 argument, by position
            1 | X = "a,b".split(sep = ",")
              |                 ^^^"#]],
    );
    check(
        r#"X = "a,b".split(",", maxsplit = 1)"#,
        &expect![[r#"
        text.star:1:22: `split`'s `maxsplit` is not in Pumpkin's Starlark: give `split` 1 argument, by position
            1 | X = "a,b".split(",", maxsplit = 1)
              |                      ^^^^^^^^"#]],
    );
    check(
        r#"X = {}.get("b", default = 2)"#,
        &expect![[r#"
        text.star:1:17: `get`'s `default` by name is not in Pumpkin's Starlark: give it by position, after the key
            1 | X = {}.get("b", default = 2)
              |                 ^^^^^^^"#]],
    );
    check(
        r#"X = sorted(["b", "a"], None)"#,
        &expect![[r#"
        text.star:1:5: `sorted` with 2 arguments is not in Pumpkin's Starlark: give it 1 argument
            1 | X = sorted(["b", "a"], None)
              |     ^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = "{-0}".format(1)"#,
        &expect![[r#"
        text.star:1:5: `{-0}`, a field that some Starlark implementations read as `{0}`, is not in Pumpkin's Starlark: write `{0}`
            1 | X = "{-0}".format(1)
              |     ^^^^^^"#]],
    );
    check(
        "X = sorted([1], None, True)",
        &expect![[r#"
        text.star:1:5: `sorted` with 3 arguments is not in Pumpkin's Starlark: give it 1 argument
            1 | X = sorted([1], None, True)
              |     ^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        "X = int()",
        &expect![[r#"
        text.star:1:5: `int` with no argument is not in Pumpkin's Starlark: give it 1 argument
            1 | X = int()
              |     ^^^^^"#]],
    );
    check(
        "X = bool()",
        &expect![[r#"
        text.star:1:5: `bool` with no argument is not in Pumpkin's Starlark: give it 1 argument
            1 | X = bool()
              |     ^^^^^^"#]],
    );
    check(
        r#"X = "a".lower()"#,
        &expect![[r#"
        text.star:1:9: `lower` of a string is not in Pumpkin's Starlark: call one of `endswith`, `format`, `join`, `replace`, `split`, `startswith`
            1 | X = "a".lower()
              |         ^^^^^
            the receiver, `"a"`, is made at text.star:1:5"#]],
    );
    check(
        "X = (1,).index(1)",
        &expect![[r#"
        text.star:1:10: a tuple has no method `index`: give it to a function, as `len`
            1 | X = (1,).index(1)
              |          ^^^^^
            the receiver, a tuple, is made at text.star:1:5"#]],
    );
    check(
        r#"X = "a".replace("a")"#,
        &expect![[r#"
            text.star:1:5: `replace` takes 2 arguments, and is given 1: give it 2 arguments
                1 | X = "a".replace("a")
                  |     ^^^^^^^^^^^^^^^^"#]],
    );
    check(
        "X = {}.get([1])",
        &expect![[r#"
            text.star:1:12: a list can't be a dict's key: key a dict with strings or numbers
                1 | X = {}.get([1])
                  |            ^^^"#]],
    );
    check(
        "X = {}.get(True)",
        &expect![[r#"
            text.star:1:12: a bool as a dict's key is not in Pumpkin's Starlark: key a dict with strings or numbers
                1 | X = {}.get(True)
                  |            ^^^^"#]],
    );
    check(
        "X = None in {}",
        &expect![[r#"
            text.star:1:10: `None` as a dict's key is not in Pumpkin's Starlark: key a dict with strings or numbers
                1 | X = None in {}
                  |          ^^
                the left operand, `None`, is made at text.star:1:5"#]],
    );
    check(
        "X = {}.get()",
        &expect![[r#"
            text.star:1:5: `get` takes 1 or 2 arguments, by position: give it a key, and a default or none
                1 | X = {}.get()
                  |     ^^^^^^^^"#]],
    );
    check(
        "X = {}.keys(1)",
        &expect![[r#"
            text.star:1:5: `keys` takes no argument, and is given 1: give it no argument
                1 | X = {}.keys(1)
                  |     ^^^^^^^^^^"#]],
    );
    check(
        r#"X = "{".format()"#,
        &expect![[r#"
            text.star:1:5: this `format` string has a `{` that opens no field: close it with `}`, or write `{{` for a `{`
                1 | X = "{".format()
                  |     ^^^"#]],
    );
    check(
        r#"X = "}".format()"#,
        &expect![[r#"
            text.star:1:5: this `format` string has a `}` that closes no field: write `}}` for a `}`
                1 | X = "}".format()
                  |     ^^^"#]],
    );
    check(
        r#"X = "{}{0}".format(1)"#,
        &expect![[r#"
            text.star:1:5: this `format` string has numbered fields and `{}` fields: number every field, or none
                1 | X = "{}{0}".format(1)
                  |     ^^^^^^^"#]],
    );
    // A field's number is decimal, whatever zeros lead it.
    check(
        r#"fail("{01}".format(1, 2))"#,
        &expect![[r#"
        text.star:1:1: fail: 2
            1 | fail("{01}".format(1, 2))
              | ^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = "{} {}".format(1)"#,
        &expect![[r#"
            text.star:1:5: `{}` names no argument of this `format` call, which has 1 by position: give it one more
                1 | X = "{} {}".format(1)
                  |     ^^^^^^^"#]],
    );
    check(
        r#"X = "{x}".format(y = 1)"#,
        &expect![[r#"
            text.star:1:5: `{x}` names no keyword argument of this `format` call: give it `x = …`
                1 | X = "{x}".format(y = 1)
                  |     ^^^^^"#]],
    );
    check(
        r#"X = "{0:x}".format(1)"#,
        &expect![[r#"
            text.star:1:5: `{0:x}` is no field of a `format` string: write `{}`, `{0}` or `{name}`
                1 | X = "{0:x}".format(1)
                  |     ^^^^^^^"#]],
    );
    // The line and paragraph separators and the bidirectional controls, which only the
    // characters themselves write.
    check(
        "X = \"{a\u{2028}b\u{2029}c\u{202a}d\u{202b}e\u{202c}f\u{202d}g\u{202e}h}\".format()",
        &expect![[r#"
            text.star:1:5: `{a<U+2028>b<U+2029>c<U+202A>d<U+202B>e<U+202C>f<U+202D>g<U+202E>h}` is no field of a `format` string: write `{}`, `{0}` or `{name}`
                1 | X = "{a\u{2028}b\u{2029}c\u{202a}d\u{202b}e\u{202c}f\u{202d}g\u{202e}h}".format()
                  |     ^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = "{a\nb\033c\177d}".format()"#,
        &expect![[r#"
            text.star:1:5: `{a\nb\033c\177d}` is no field of a `format` string: write `{}`, `{0}` or `{name}`
                1 | X = "{a\nb\033c\177d}".format()
                  |     ^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = "{}".format([1])"#,
        &expect![[r#"
            text.star:1:17: `format` of a list is not in Pumpkin's Starlark: give it a number or a string
                1 | X = "{}".format([1])
                  |                 ^^^"#]],
    );
    check(
        r#"X = "0123456789012345678901234567890123456789012345678901234567890123" + 1"#,
        &expect![[r#"
            text.star:1:72: `+` can't take a string and an int: give it two numbers, two strings, two lists or two tuples
                1 | X = "0123456789012345678901234567890123456789012345678901234567890123" + 1
                  |                                                                        ^
                the left operand, `"012345678901234567890123456789012345678901234567890123456789…"`, is made at text.star:1:5
                the right operand, `1`, is made at text.star:1:74"#]],
    );
    check(
        r#"
load("//lib/deep.star", "DEEP")
X = [[DEEP[0]] for x in [1]]"#,
        &expect![[r#"
            text.star:2:5: this makes a value nested 129 deep: nest lists, tuples, dicts and functions' defaults at most 128 deep
                2 | X = [[DEEP[0]] for x in [1]]
                  |     ^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"
load("//lib/deep.star", "DEEP")
X = {1: [DEEP[0]] for x in [1]}"#,
        &expect![[r#"
            text.star:2:5: this makes a value nested 129 deep: nest lists, tuples, dicts and functions' defaults at most 128 deep
                2 | X = {1: [DEEP[0]] for x in [1]}
                  |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"
load("//lib/deep.star", "DEEP")
X = {1: DEEP[0]}.items()"#,
        &expect![[r#"
            text.star:2:5: this makes a value nested 129 deep: nest lists, tuples, dicts and functions' defaults at most 128 deep
                2 | X = {1: DEEP[0]}.items()
                  |     ^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = {"a": 1, "a": 2}"#,
        &expect![[r#"
            text.star:1:14: `"a"` is a key of this dict already: give each key once
                1 | X = {"a": 1, "a": 2}
                  |              ^^^
                it is first given at text.star:1:6"#]],
    );
    check(
        r#"X = {1: "a", 1.0: "b"}"#,
        &expect![[r#"
            text.star:1:14: `1.0` is a key of this dict already: give each key once
                1 | X = {1: "a", 1.0: "b"}
                  |              ^^^
                it is first given at text.star:1:6"#]],
    );
    check(
        r#"load("//lib/../task.star", "X")"#,
        &expect![[r#"
            text.star:1:6: `//lib/../task.star` is no label: after `//`, write `/`-separated components of ASCII letters, digits, `.`, `_` and `-`, none of them empty, `.` or `..`
                1 | load("//lib/../task.star", "X")
                  |      ^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib/private.star", "X")
#- //lib/private.star
"""A library whose one global is private to it."""

_PRIVATE = "private to this file"
"#,
        &expect![[r#"
            text.star:1:28: `//lib/private.star` binds no global that other files can load: bind `X` in it
                1 | load("//lib/private.star", "X")
                  |                            ^^^"#]],
    );
    check(
        r#"workflow(name = "")"#,
        &expect![[r#"
            text.star:1:1: `workflow` lacks `roles`, `final`, `deadline_seconds`: give them
                1 | workflow(name = "")
                  | ^^^^^^^^^^^^^^^^^^^"#]],
    );
    // Values the IR's types refuse, in the language's words.
    check(
        r#"workflow(name = "n/x", roles = [1], final = None, deadline_seconds = 3600)"#,
        &expect![[r#"
            text.star:1:33: an element of `roles` can't be an int: expected a dict
                1 | workflow(name = "n/x", roles = [1], final = None, deadline_seconds = 3600)
                  |                                 ^
                the IR's /roles/0"#]],
    );
    check(
        r#"workflow(roles = [{"user": {"uid": -1}}], name = "", final = None, deadline_seconds = 3600)"#,
        &expect![[r#"
            text.star:1:36: `uid` can't be `-1`: expected an int from 1 to 4294967295
                1 | workflow(roles = [{"user": {"uid": -1}}], name = "", final = None, deadline_seconds = 3600)
                  |                                    ^^
                the IR's /roles/0/user/uid"#]],
    );
    check(
        r#"workflow(roles = [{"user": {"uid": 1.5}}], name = "", final = None, deadline_seconds = 3600)"#,
        &expect![[r#"
            text.star:1:36: `uid` can't be a float: expected an int
                1 | workflow(roles = [{"user": {"uid": 1.5}}], name = "", final = None, deadline_seconds = 3600)
                  |                                    ^^^
                the IR's /roles/0/user/uid"#]],
    );
    check(
        r#"workflow(roles = [{"runtime": "agent"}], name = "", final = None, deadline_seconds = 3600)"#,
        &expect![[r#"
            text.star:1:31: `runtime` can't be `"agent"`: expected a dict that gives the kind its value, as `{"agent": …}`
                1 | workflow(roles = [{"runtime": "agent"}], name = "", final = None, deadline_seconds = 3600)
                  |                               ^^^^^^^
                the IR's /roles/0/runtime"#]],
    );
    check(
        r#"workflow(roles = [{"runtime": {"agent": {"finalize_grace_seconds": -1}}}], name = "", final = None, deadline_seconds = 3600)"#,
        &expect![[r#"
            text.star:1:68: `finalize_grace_seconds` can't be `-1`: expected an int of 0 or more
                1 | workflow(roles = [{"runtime": {"agent": {"finalize_grace_seconds": -1}}}], name = "", final = None, deadline_seconds = 3600)
                  |                                                                    ^^
                the IR's /roles/0/runtime/agent/finalize_grace_seconds"#]],
    );
    // Problems past the first 20.
    check(
        "X = [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u]",
        &expect![[r#"
            text.star:1:6: `a` is not defined: bind it in this file, or load it
                1 | X = [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u]
                  |      ^
            text.star:1:9: `b` is not defined: bind it in this file, or load it
                1 | X = [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u]
                  |         ^
            text.star:1:12: `c` is not defined: bind it in this file, or load it
                1 | X = [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u]
                  |            ^
            text.star:1:15: `d` is not defined: bind it in this file, or load it
                1 | X = [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u]
                  |               ^
            text.star:1:18: `e` is not defined: bind it in this file, or load it
                1 | X = [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u]
                  |                  ^
            text.star:1:21: `f` is not defined: bind it in this file, or load it
                1 | X = [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u]
                  |                     ^
            text.star:1:24: `g` is not defined: bind it in this file, or load it
                1 | X = [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u]
                  |                        ^
            text.star:1:27: `h` is not defined: bind it in this file, or load it
                1 | X = [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u]
                  |                           ^
            text.star:1:30: `i` is not defined: bind it in this file, or load it
                1 | X = [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u]
                  |                              ^
            text.star:1:33: `j` is not defined: bind it in this file, or load it
                1 | X = [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u]
                  |                                 ^
            text.star:1:36: `k` is not defined: bind it in this file, or load it
                1 | X = [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u]
                  |                                    ^
            text.star:1:39: `l` is not defined: bind it in this file, or load it
                1 | X = [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u]
                  |                                       ^
            text.star:1:42: `m` is not defined: bind it in this file, or load it
                1 | X = [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u]
                  |                                          ^
            text.star:1:45: `n` is not defined: bind it in this file, or load it
                1 | X = [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u]
                  |                                             ^
            text.star:1:48: `o` is not defined: bind it in this file, or load it
                1 | X = [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u]
                  |                                                ^
            text.star:1:51: `p` is not defined: bind it in this file, or load it
                1 | X = [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u]
                  |                                                   ^
            text.star:1:54: `q` is not defined: bind it in this file, or load it
                1 | X = [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u]
                  |                                                      ^
            text.star:1:57: `r` is not defined: bind it in this file, or load it
                1 | X = [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u]
                  |                                                         ^
            text.star:1:60: `s` is not defined: bind it in this file, or load it
                1 | X = [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u]
                  |                                                            ^
            text.star:1:63: `t` is not defined: bind it in this file, or load it
                1 | X = [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u]
                  |                                                               ^
            and 1 more problem: fix these first, then run again"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshots"
)]
fn values() {
    check(
        r#"
X = " ".join([str(int(b)) for b in [1 < 2, 2 <= 1, 3 > 2, 2 >= 3, "a" < "b", "ab" <= "a", 1 == 1, 1 != 1, [1, "a"] == [1, "a"], [1] == (1,), {"a": 1, "b": 2} == {"b": 2, "a": 1}, {"a": 1} != {"a": 2}, None == None, len == len, 1 == "1", True == True, True == False, (1,) == (1,), [1] == [2], [1] == [1, 2], {"a": 1} == {"b": 1}]])
fail(X)"#,
        &expect![[r#"
            text.star:2:1: fail: 1 0 1 0 1 0 1 0 1 0 1 1 1 1 0 1 0 1 0 0 0
                2 | fail(X)
                  | ^^^^^^^"#]],
    );
    check(
        r#"
X = " ".join([str(int(b)) for b in [1 in [1, 2], 3 in (1, 2), "a" in {"a": 1}, 1 in {"a": 1}, "bc" in "abcd", "x" not in "abc", 2 not in [2]]])
fail(X)"#,
        &expect![[r#"
            text.star:2:1: fail: 1 0 1 0 1 1 0
                2 | fail(X)
                  | ^^^^^^^"#]],
    );
    check(
        r#"
X = " ".join([str(x) for x in [0 or 2, 1 and 3, 0 and 4, 5 or 6, int(not 0), int(not [1]), int(bool("")), int(bool({"a": 1})), int(False), int(True), 7 if [] else 8, 9 if "s" else 10, 11 if 0 else 12 if 1 else 13]])
fail(X)"#,
        &expect![[r#"
            text.star:2:1: fail: 2 3 0 5 1 0 0 1 0 1 8 9 12
                2 | fail(X)
                  | ^^^^^^^"#]],
    );
    check(
        r#"
X = " ".join([str(x) for x in [[1, 2, 3][-1], [1, 2, 3][0], (4, 5)[-2], {"a": 6}["a"], {7: 8}[7]]])
fail(X)"#,
        &expect![[r#"
            text.star:2:1: fail: 3 1 4 6 8
                2 | fail(X)
                  | ^^^^^^^"#]],
    );
    check(
        r#"
X = " ".join([str(len([1, 2])), str(len((1,))), str(len({})), str(int("-12") + int("+3") + int("007")), str(int(True) + int(7)), ",".join(sorted(["b", "c", "a"])), ",".join([str(x) for x in sorted((3, -1, 2))]), ",".join(sorted({"y": 1, "x": 2}))])
fail(X)"#,
        &expect![[r#"
            text.star:2:1: fail: 2 1 0 -2 8 a,b,c -1,2,3 x,y
                2 | fail(X)
                  | ^^^^^^^"#]],
    );
    // A comprehension iterates over a dict's keys in the order they were first inserted.
    check(
        r#"
D = {"b": 1, "a": 2, "c": 3} | {"a": 4, "d": 5}
X = ",".join([k for k in D] + [k + str(v) for k, v in {k: D[k] for k in D}.items()])
fail(X)"#,
        &expect![[r#"
            text.star:3:1: fail: b,a,c,d,b1,a4,c3,d5
                3 | fail(X)
                  | ^^^^^^^"#]],
    );
    check(
        r#"
D = {"b": 1, "a": 2}
X = " ".join([",".join(D.keys()), ",".join([str(v) for v in D.values()]), ",".join([k + str(v) for k, v in D.items()]), str(D.get("a")), str(D.get("c", 3)), "none" if D.get("c") == None else "some", ",".join([k + str(v) for k, v in (D | {"a": 5, "c": 6}).items()])])
fail(X)"#,
        &expect![[r#"
            text.star:3:1: fail: b,a 1,2 b1,a2 2 3 none b1,a5,c6
                3 | fail(X)
                  | ^^^^^^^"#]],
    );
    check(
        r#"
X = " | ".join(["{} {x} {}{long_name}".format(1, "two", x = 3, long_name = 4), "{1}{0}{{}}".format("a", "b"), "-".join(("p", "q")), ",".join("a/b//c".split("/")), str(int("ab".startswith("a"))) + str(int("ab".endswith("a"))), "banana".replace("an", "_")])
fail(X)"#,
        &expect![[r#"
            text.star:2:1: fail: 1 3 two4 | ba{} | p-q | a,b,,c | 10 | b__a
                2 | fail(X)
                  | ^^^^^^^"#]],
    );
    check(
        r#"
X = " ".join([",".join([str(x * y) for x in [1, 2, 3] if x > 1 for y in [10, 20] if y < 20]), ",".join([k + v for k, v in {k: v for k, v in [("a", "1"), ("b", "2"), ("a", "3")]}.items()]), ",".join([str(x) for x in [x + 1 for x in [1, 2]]]), ",".join([a + b for (a, b) in [("p", "q")]])])
fail(X)"#,
        &expect![[r#"
            text.star:2:1: fail: 20,30 a3,b2 2,3 pq
                2 | fail(X)
                  | ^^^^^^^"#]],
    );
    check(
        r#"
x = "outer"
Y = [x for x in ["inner"]]
fail(x + " " + Y[0])"#,
        &expect![[r#"
            text.star:3:1: fail: outer inner
                3 | fail(x + " " + Y[0])
                  | ^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        "X = [1 // 0 for x in [] for y in z for z in ()]",
        &expect!["text.star: this file calls no `workflow()`: call it once, with the workflow"],
    );
    check(
        "X = [y for x in []]",
        &expect![[r#"
            text.star:1:6: `y` is not defined: bind it in this file, or load it
                1 | X = [y for x in []]
                  |      ^"#]],
    );
    check(
        "
w = [9]
X = [y for y in [1] for z in w for w in [[1]]]",
        &expect![[r#"
            text.star:2:30: `w` is used before it is bound: bind it above this use
                2 | X = [y for y in [1] for z in w for w in [[1]]]
                  |                              ^"#]],
    );
    check(
        r#"
X = " ".join([str(x) for x in [1.5, .5, 1., 01.5, 1e3, 0.0, -0.0, 123456.7, 1e6, 1.2345678e7, 1e-5, 9e-6, 1e-7, 5e-324, 1e100, 0.1 + 0.2, 1e308 * 10, -1e308 * 10]])
fail(X)"#,
        &expect![[r#"
            text.star:2:1: fail: 1.5 0.5 1.0 1.5 1000.0 0.0 -0.0 123456.7 1e+06 1.2345678e+07 0.00001 9e-06 1e-07 5e-324 1e+100 0.30000000000000004 +inf -inf
                2 | fail(X)
                  | ^^^^^^^"#]],
    );
    check(
        r#"
X = " ".join([str(x) for x in [1 / 2, 7 / 7, 2 * 1.5, 1 + 0.5, 3 - 0.5, 7 // 2.0, -7.5 // 2, -7.5 % 2, 7.5 % -2, -4.0 % 2, 5 % float("inf"), -5 % float("inf"), -(1.5), -0.0 // 1]])
fail(X)"#,
        &expect![[r#"
            text.star:2:1: fail: 0.5 1.0 3.0 1.5 2.5 3.0 -4.0 0.5 -0.5 0.0 5.0 +inf -1.5 -0.0
                2 | fail(X)
                  | ^^^^^^^"#]],
    );
    check(
        r#"
X = [1 == 1.0, 1.0 != 1, 9007199254740993 > 9007199254740992.0, 9007199254740993 == 9007199254740992.0, 9007199254740992 == 9007199254740992.0, 1.5 < 2, 2 <= 1.5, -0.5 < 0, -0.0 == 0, 9223372036854775807 < 9223372036854775808.0, -9223372036854775807 - 1 == -9223372036854775808.0, 1e19 > 9223372036854775807, float("-inf") < -9223372036854775807, float("nan") == float("nan"), float("nan") > float("inf"), float("nan") > 9223372036854775807, 1.0 in [1], [1, 2.0] == [1.0, 2], bool(0.0), bool(-0.0), bool(float("nan"))]
fail(" ".join([str(int(x)) for x in X]))"#,
        &expect![[r#"
            text.star:2:1: fail: 1 0 1 0 1 1 0 1 1 1 1 1 1 1 1 1 1 1 0 0 1
                2 | fail(" ".join([str(int(x)) for x in X]))
                  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"
D = {1: "a"} | {1.0: "b"}
X = [{1.0: "x"}[1], {0: "z"}[-0.0], str(int(float("nan") in {float("nan"): 1})), D[1], str(D.keys()[0]), str({1.0: "y"}.keys()[0])]
fail(" ".join(X))"#,
        &expect![[r#"
            text.star:3:1: fail: x z 1 b 1 1.0
                3 | fail(" ".join(X))
                  | ^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"
X = " ".join([str(x) for x in sorted([3, 1.5, float("nan"), -0.0, 0, float("-inf"), 2])])
fail(X)"#,
        &expect![[r#"
            text.star:2:1: fail: -inf -0.0 0 1.5 2 3 nan
                2 | fail(X)
                  | ^^^^^^^"#]],
    );
    check(
        r#"
X = " ".join([str(x) for x in [float(), float(3), float(True), float(1.5), float("1e3"), float("-inf"), float("+Infinity"), float("nan"), float(9007199254740993), int(43.12), int(-2.5), int(-9223372036854775808.0), int(9223372036854774784.0)]])
fail(X)"#,
        &expect![[r#"
            text.star:2:1: fail: 0.0 3.0 1.0 1.5 1000.0 -inf +inf nan 9.007199254740992e+15 43 -2 -9223372036854775808 9223372036854774784
                2 | fail(X)
                  | ^^^^^^^"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshots"
)]
fn bounds() {
    let steps = |steps| Bounds {
        steps,
        ..Bounds::WORKFLOW
    };
    // 11 steps make the list, and 116 the second line, 100 of them the comprehension's element.
    let counted = "L = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]\nfail(len([0 for a in L for b in L]))";
    check_within(
        steps(127),
        counted,
        &expect![[r#"
        text.star:2:1: fail: 100
            2 | fail(len([0 for a in L for b in L]))
              | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check_within(
        steps(126),
        counted,
        &expect![[r#"
        text.star:2:11: this evaluation runs past 126 steps, an expression's evaluation each: compute less, as with fewer `for` clauses or smaller lists
            2 | fail(len([0 for a in L for b in L]))
              |           ^"#]],
    );
    let bytes = |bytes| Bounds {
        bytes,
        ..Bounds::WORKFLOW
    };
    // Doubling a list, which would ask for gigabytes in 26 doublings, ends at the first that passes
    // the bound: the list's 80 bytes, then 160, 320 and 640.
    let doubled = r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def double(v):
    v = v + v
    v = v + v
    v = v + v
    v = v + v
    v = v + v
    v = v + v
    return v

G = double([0])"#;
    check_within(
        bytes(1000),
        doubled,
        &expect![[r#"
        lib.star:4:11: this evaluation's strings, lists, tuples and dicts take more than 1000 bytes: make smaller values, as with fewer `+` of long ones
            4 |     v = v + v
              |           ^
            in `double()`, called at lib.star:10:5"#]],
    );
    // Each of these makes values of exactly the bytes it is given, and is refused one fewer: 16
    // of a string's text, 8 of what `replace` makes, 240 of a list's three elements and 10 of
    // what `join` makes, 164 of two strings' text and their list's elements, 5 of what `format`
    // makes; a string literal's text is not counted.
    check_within(
        bytes(16),
        r#"
X = "abcdefgh" + "ijklmnop"
fail(int(X == ""))"#,
        &expect![[r#"
        text.star:2:1: fail: 0
            2 | fail(int(X == ""))
              | ^^^^^^^^^^^^^^^^^^"#]],
    );
    check_within(
        bytes(15),
        r#"
X = "abcdefgh" + "ijklmnop"
fail(int(X == ""))"#,
        &expect![[r#"
        text.star:1:16: this evaluation's strings, lists, tuples and dicts take more than 15 bytes: make smaller values, as with fewer `+` of long ones
            1 | X = "abcdefgh" + "ijklmnop"
              |                ^"#]],
    );
    check_within(
        bytes(8),
        r#"
X = "aabaab".replace("aa", "xyz")
fail(int(X == ""))"#,
        &expect![[r#"
        text.star:2:1: fail: 0
            2 | fail(int(X == ""))
              | ^^^^^^^^^^^^^^^^^^"#]],
    );
    check_within(
        bytes(7),
        r#"
X = "aabaab".replace("aa", "xyz")
fail(int(X == ""))"#,
        &expect![[r#"
        text.star:1:5: this evaluation's strings, lists, tuples and dicts take more than 7 bytes: make smaller values, as with fewer `+` of long ones
            1 | X = "aabaab".replace("aa", "xyz")
              |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check_within(
        bytes(250),
        r#"
X = "--".join(["ab", "cd", "ef"])
fail(int(X == ""))"#,
        &expect![[r#"
        text.star:2:1: fail: 0
            2 | fail(int(X == ""))
              | ^^^^^^^^^^^^^^^^^^"#]],
    );
    check_within(
        bytes(249),
        r#"
X = "--".join(["ab", "cd", "ef"])
fail(int(X == ""))"#,
        &expect![[r#"
        text.star:1:5: this evaluation's strings, lists, tuples and dicts take more than 249 bytes: make smaller values, as with fewer `+` of long ones
            1 | X = "--".join(["ab", "cd", "ef"])
              |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check_within(
        bytes(164),
        r#"
X = "ab--cd".split("--")
fail(int(X == ""))"#,
        &expect![[r#"
            text.star:2:1: fail: 0
                2 | fail(int(X == ""))
                  | ^^^^^^^^^^^^^^^^^^"#]],
    );
    check_within(
        bytes(163),
        r#"
X = "ab--cd".split("--")
fail(int(X == ""))"#,
        &expect![[r#"
        text.star:1:5: this evaluation's strings, lists, tuples and dicts take more than 163 bytes: make smaller values, as with fewer `+` of long ones
            1 | X = "ab--cd".split("--")
              |     ^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check_within(
        bytes(5),
        r#"
X = "{}-{}".format("ab", "cd")
fail(int(X == ""))"#,
        &expect![[r#"
        text.star:2:1: fail: 0
            2 | fail(int(X == ""))
              | ^^^^^^^^^^^^^^^^^^"#]],
    );
    check_within(
        bytes(4),
        r#"
X = "{}-{}".format("ab", "cd")
fail(int(X == ""))"#,
        &expect![[r#"
        text.star:1:5: this evaluation's strings, lists, tuples and dicts take more than 4 bytes: make smaller values, as with fewer `+` of long ones
            1 | X = "{}-{}".format("ab", "cd")
              |     ^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
}

#[test]
fn chains() {
    // One operator more than an expression may nest deep, so that each of these would be refused
    // if its operators nested.
    check(
        &format!("X = 1{}\nfail(str(X))", " + 1".repeat(129)),
        &expect![[r#"
            text.star:2:1: fail: 130
                2 | fail(str(X))
                  | ^^^^^^^^^^^^"#]],
    );
    check(
        &format!("X = 0{} or 3\nfail(str(X))", " or 0".repeat(129)),
        &expect![[r#"
            text.star:2:1: fail: 3
                2 | fail(str(X))
                  | ^^^^^^^^^^^^"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshots"
)]
fn functions() {
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(a, b = 2, *, c, d = 4):
    return "{} {} {} {}".format(a, b, c, d)
fail(f(1, c = 3) + ", " + f(1, 5, d = 6, c = 7) + ", " + f(c = 8, a = 9))"#,
        &expect![[r#"
            lib.star:3:1: fail: 1 2 3 4, 1 5 7 6, 9 2 8 4
                3 | fail(f(1, c = 3) + ", " + f(1, 5, d = 6, c = 7) + ", " + f(c = 8, a = 9))
                  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def sign(n):
    if n < 0:
        return "negative"
    elif n == 0:
        return "zero"
    else:
        return "positive"
fail(sign(-1) + " " + sign(0) + " " + sign(1))"#,
        &expect![[r#"
            lib.star:8:1: fail: negative zero positive
                8 | fail(sign(-1) + " " + sign(0) + " " + sign(1))
                  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(n):
    total = n
    if n > 0:
        if n > 5:
            total = total * 2
        total = total + 1
    return total
def g():
    pass
fail("{} {} {} {}".format(f(-1), f(2), f(6), "none" if g() == None else "some"))"#,
        &expect![[r#"
            lib.star:10:1: fail: -1 3 13 none
                10 | fail("{} {} {} {}".format(f(-1), f(2), f(6), "none" if g() == None else "some"))
                   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(): return 1
def g(): return 1
h = f
fail(" ".join([str(int(f == h)), str(int(f == g)), str(h())]))"#,
        &expect![[r#"
            lib.star:4:1: fail: 1 0 1
                4 | fail(" ".join([str(int(f == h)), str(int(f == g)), str(h())]))
                  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(n):
    if n: return "a"
    return "b"
fail(f(1) + f(0))"#,
        &expect![[r#"
            lib.star:4:1: fail: ab
                4 | fail(f(1) + f(0))
                  | ^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f():
    return K
X = f()
K = 1"#,
        &expect![[r#"
            lib.star:2:12: `K` is used before it is bound: bind it above this use
                2 |     return K
                  |            ^
                in `f()`, called at lib.star:3:5"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
y = 1
def f():
    z = y
    y = 2
    return z
X = f()"#,
        &expect![[r#"
            lib.star:3:9: `y` is used before it is bound: bind it above this use
                3 |     z = y
                  |         ^
                in `f()`, called at lib.star:6:5"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(x = K):
    return x
K = 1
X = f()"#,
        &expect![[r#"
            lib.star:1:11: `K` is used before it is bound: bind it above this use
                1 | def f(x = K):
                  |           ^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f():
    return f()
X = f()"#,
        &expect![[r#"
            lib.star:2:12: this calls `f` while it runs, and a Starlark function never calls itself, even through others: compute the value without calling `f` again
                2 |     return f()
                  |            ^^^
                in `f()`, called at lib.star:3:5"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f():
    return g()
def g():
    return f()
X = f()"#,
        &expect![[r#"
            lib.star:4:12: this calls `f` while it runs, and a Starlark function never calls itself, even through others: compute the value without calling `f` again
                4 |     return f()
                  |            ^^^
                in `g()`, called at lib.star:2:12
                in `f()`, called at lib.star:5:5"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(a):
    return a
X = f(1, 2)"#,
        &expect![[r#"
            lib.star:3:10: `f` takes 1 argument by position: name the rest
                3 | X = f(1, 2)
                  |          ^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(*, a):
    return a
X = f(1)"#,
        &expect![[r#"
            lib.star:3:7: `f` takes its arguments by name only: name each, as `a = …`
                3 | X = f(1)
                  |       ^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f():
    pass
X = f(1)"#,
        &expect![[r#"
            lib.star:3:7: `f` takes no argument: drop it
                3 | X = f(1)
                  |       ^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f():
    pass
X = f(a = 1)"#,
        &expect![[r#"
            lib.star:3:7: `a` is no parameter of `f`, which takes no argument: drop it
                3 | X = f(a = 1)
                  |       ^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(a):
    return a
X = f(1, a = 2)"#,
        &expect![[r#"
            lib.star:3:10: `a` is given by position already: give it once
                3 | X = f(1, a = 2)
                  |          ^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(a):
    return a
X = f(b = 1)"#,
        &expect![[r#"
            lib.star:3:7: `b` is no parameter of `f`: expected one of `a`
                3 | X = f(b = 1)
                  |       ^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(a, b):
    return a
X = f(b = 1, 2)"#,
        &expect![[r#"
            lib.star:3:14: a positional argument follows a keyword argument: put positional arguments first
                3 | X = f(b = 1, 2)
                  |              ^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(a):
    return a
X = f(a = 1, a = 2)"#,
        &expect![[r#"
            lib.star:3:14: `a` is given twice: give it once
                3 | X = f(a = 1, a = 2)
                  |              ^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def codex(model, *, effort = "low"):
    return "codex {} {}".format(model, effort)
def claude(model, *, thinking):
    return "claude {} {}".format(model, thinking)
HARNESSES = {"codex": (codex, {"effort": "high"}), "claude": (claude, {"thinking": "on"})}
OPTIONS = {"a": 1, "b": 2}
PASSED = [h("m", **o) for h, o in HARNESSES.values()] + ["{a} {b}".format(**OPTIONS)]
fail(", ".join(PASSED + [str(len([0], **{}))]))"#,
        &expect![[r#"
            lib.star:8:1: fail: codex m high, claude m on, 1 2, 1
                8 | fail(", ".join(PASSED + [str(len([0], **{}))]))
                  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(a):
    return a
X = f(**{"b": 1})"#,
        &expect![[r#"
            lib.star:3:9: `b` is no parameter of `f`: expected one of `a`
                3 | X = f(**{"b": 1})
                  |         ^^^^^^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(a, *, b):
    return a
X = f(1, **{})"#,
        &expect![[r#"
            lib.star:3:5: `f` lacks `b`: give it
                3 | X = f(1, **{})
                  |     ^^^^^^^^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(a, *, b):
    return a
X = f(1, b = 2, **{"b": 3})"#,
        &expect![[r#"
            lib.star:3:19: `b` is given by name already, and again with `**`: give it once
                3 | X = f(1, b = 2, **{"b": 3})
                  |                   ^^^^^^^^
                it is first given at lib.star:3:10"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
OPTIONS = [1]
X = len("a", **OPTIONS)"#,
        &expect![[r#"
            lib.star:2:16: a list can't be passed with `**`: pass a dict of keyword arguments
                2 | X = len("a", **OPTIONS)
                  |                ^^^^^^^
                the value, a list, is made at lib.star:1:11"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
OPTIONS = {1: 2}
X = len([], **OPTIONS)"#,
        &expect![[r#"
            lib.star:2:15: `1`, a key of the dict passed with `**`, is no string: key each keyword argument by its name
                2 | X = len([], **OPTIONS)
                  |               ^^^^^^^
                the key, `1`, is made at lib.star:1:12"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(a, b, *, c):
    pass
X = f()"#,
        &expect![[r#"
            lib.star:3:5: `f` lacks `a`, `b`, `c`: give them
                3 | X = f()
                  |     ^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def check(n):
    if n > 3:
        fail("{} is over 3: give 3 or less".format(n))
    return n
X = check(4)"#,
        &expect![[r#"
            lib.star:3:9: fail: 4 is over 3: give 3 or less
                3 |         fail("{} is over 3: give 3 or less".format(n))
                  |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `check()`, called at lib.star:5:5"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def make():
    workflow(name = "n/x", roles = [], final = None, deadline_seconds = 3600)
X = make()"#,
        &expect![[r#"
            lib.star:2:5: `workflow()` is called while `//lib.star` loads: call it in the entry file, which loads `//lib.star`
                2 |     workflow(name = "n/x", roles = [], final = None, deadline_seconds = 3600)
                  |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `make()`, called at lib.star:3:5"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(t, v):
    return t.format(y = v)
X = f("a{x}b", 1)"#,
        &expect![[r#"
            lib.star:2:12: `{x}` names no keyword argument of this `format` call: give it `x = …`
                2 |     return t.format(y = v)
                  |            ^
                the string, `"a{x}b"`, is made at lib.star:3:7
                in `f()`, called at lib.star:3:5"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
load("//lib/deep.star", "DEEP")
def f(x = DEEP):
    return x
X = 1"#,
        &expect![[r#"
            lib.star:2:5: this makes a value nested 129 deep: nest lists, tuples, dicts and functions' defaults at most 128 deep
                2 | def f(x = DEEP):
                  |     ^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
X = {}
Y = X.update({})"#,
        &expect![[r#"
            lib.star:2:7: `update`, which would change the dict, is not in Pumpkin's Starlark: make a new dict with `|`, as values never change
                2 | Y = X.update({})
                  |       ^^^^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(*args):
    pass"#,
        &expect![[r#"
            lib.star:1:7: `*args` is not in Pumpkin's Starlark: take the values in a list
                1 | def f(*args):
                  |       ^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(**kwargs):
    pass"#,
        &expect![[r#"
            lib.star:1:7: `**kwargs`, which would let a misspelt keyword pass unseen, is not in Pumpkin's Starlark: name each parameter
                1 | def f(**kwargs):
                  |       ^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(*):
    pass"#,
        &expect![[r#"
            lib.star:1:7: no parameter follows this `*`: name a keyword-only one after it, or drop it
                1 | def f(*):
                  |       ^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(*, a, *, b):
    pass"#,
        &expect![[r#"
            lib.star:1:13: this is a second `*`: mark the keyword-only parameters with one
                1 | def f(*, a, *, b):
                  |             ^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(a = 1, b):
    pass"#,
        &expect![[r#"
            lib.star:1:14: `b` has no default, and follows `a`, which has one: give it a default, or put it before `a`
                1 | def f(a = 1, b):
                  |              ^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(a, a):
    pass"#,
        &expect![[r#"
            lib.star:1:10: `a` is a parameter already: name each once
                1 | def f(a, a):
                  |          ^
                it is first named at lib.star:1:7"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f() - 1:
    return 1"#,
        &expect![[r#"
            lib.star:1:9: unexpected `-`: expected `:`
                1 | def f() - 1:
                  |         ^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f():
    def g():
        pass"#,
        &expect![[r#"
            lib.star:2:5: a `def` inside a function is not in Pumpkin's Starlark: write it at the top level, passing it what it uses
                2 |     def g():
                  |     ^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f():
    load("//lib/parts.star", "FINAL")"#,
        &expect![[r#"
            lib.star:2:5: a `load` stands only at a file's top level, above its other statements: move it there
                2 |     load("//lib/parts.star", "FINAL")
                  |     ^^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f():
    for x in []:
        pass"#,
        &expect![[r#"
            lib.star:2:5: a `for` statement is not in Pumpkin's Starlark: build the list with a comprehension, as `[f(x) for x in xs]`
                2 |     for x in []:
                  |     ^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f():
    x = 1
    x += 1"#,
        &expect![[r#"
            lib.star:3:7: `+=` is not in Pumpkin's Starlark: bind the result with `=`
                3 |     x += 1
                  |       ^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f():
    x = 1
        y = 2"#,
        &expect![[r#"
            lib.star:3:1: this line is indented more than the block it is in: indent it as the line above
                3 |         y = 2
                  | ^^^^^^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f():
return 1"#,
        &expect![[r#"
            lib.star:2:1: unexpected `return`: expected the block's lines, indented
                2 | return 1
                  | ^^^^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f():
    return y"#,
        &expect![[r#"
            lib.star:2:12: `y` is not defined: bind it in this file, or load it
                2 |     return y
                  |            ^"#]],
    );
}

/// The entry of a task whose package's recipes build its images, as `lib/images.star` writes
/// its workflow, which the cases below give `--arg`s.
const IMAGES: &str = r#"#- //tasks/images/task.star
"""A neutral task whose package's recipes build its images, as a library writes its workflow."""

load("//lib/images.star", "images_task")

images_task(agent = {"cpu_millis": 4000, "memory_bytes": 8 * GiB})
"#;

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshots"
)]
fn annotations() {
    // A workflow's evaluation makes no handle, and no name gives one.
    check(
        r#"
#- //text.star
load("//lib.star", "f")
f(Target)
#- //lib.star
def f(target: Target):
    pass
"#,
        &expect![[r#"
            text.star:2:3: `Target` is not defined: bind it in this file, or load it
                2 | f(Target)
                  |   ^^^^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(n: int, x: float, s: str, z: None, xs: list[int], d: dict[str, list[float]], b: bool = False) -> str:
    return s
def g() -> None:
    pass
f(1, 2.5, "fits", g(), [1, 2], {"a": [1.5]}, b = True)
fail(f(1, 2.5, "fits", None, [], {}))"#,
        &expect![[r#"
            lib.star:6:1: fail: fits
                6 | fail(f(1, 2.5, "fits", None, [], {}))
                  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(n: int):
    pass
f("x")"#,
        &expect![[r#"
            lib.star:3:1: `f`'s `n` can't be `"x"`: give an int
                3 | f("x")
                  | ^^^^^^
                the argument, `"x"`, is made at lib.star:3:3"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(x: float):
    pass
f(1)"#,
        &expect![[r#"
            lib.star:3:1: `f`'s `x` can't be `1`: give a float
                3 | f(1)
                  | ^^^^
                the argument, `1`, is made at lib.star:3:3"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(n: int):
    pass
f(True)"#,
        &expect![[r#"
            lib.star:3:1: `f`'s `n` can't be `True`: give an int
                3 | f(True)
                  | ^^^^^^^
                the argument, `True`, is made at lib.star:3:3"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(xs: list[int]):
    pass
f(xs = [1, "a"])"#,
        &expect![[r#"
            lib.star:3:1: an element of `f`'s `xs` can't be `"a"`: give an int
                3 | f(xs = [1, "a"])
                  | ^^^^^^^^^^^^^^^^
                the element, `"a"`, is made at lib.star:3:12"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(d: dict[str, int]):
    pass
f({1: 2})"#,
        &expect![[r#"
            lib.star:3:1: a key of `f`'s `d` can't be `1`: give a string
                3 | f({1: 2})
                  | ^^^^^^^^^
                the key, `1`, is made at lib.star:3:4"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(d: dict[str, list[float]]):
    pass
f({"a": [1]})"#,
        &expect![[r#"
            lib.star:3:1: an element of a value of `f`'s `d` can't be `1`: give a float
                3 | f({"a": [1]})
                  | ^^^^^^^^^^^^^
                the element, `1`, is made at lib.star:3:10"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(a: int, b):
    pass
f("x")"#,
        &expect![[r#"
            lib.star:3:1: `f` lacks `b`: give it
                3 | f("x")
                  | ^^^^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(n) -> int:
    if n:
        return "x"
    return n
def g():
    return f(1)
g()"#,
        &expect![[r#"
            lib.star:3:9: `f` can't return `"x"`: it returns an int
                3 |         return "x"
                  |         ^^^^^^
                in `f()`, called at lib.star:6:12
                in `g()`, called at lib.star:7:1"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f() -> int:
    pass
f()"#,
        &expect![[r#"
            lib.star:1:5: `f` can't return `None`: it returns an int
                1 | def f() -> int:
                  |     ^
                in `f()`, called at lib.star:3:1"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f() -> list[int]:
    return [1, "a"]
f()"#,
        &expect![[r#"
            lib.star:2:5: an element of what `f` returns can't be `"a"`: give an int
                2 |     return [1, "a"]
                  |     ^^^^^^
                the element, `"a"`, is made at lib.star:2:16
                in `f()`, called at lib.star:3:1"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def submit(finalize: bool = 0):
    pass"#,
        &expect![[r#"
            lib.star:1:29: `finalize` is a bool, and can't default to `0`: give a bool
                1 | def submit(finalize: bool = 0):
                  |                             ^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
NO = 0
def submit(finalize: bool = NO):
    pass"#,
        &expect![[r#"
            lib.star:2:29: `finalize` is a bool, and can't default to `0`: give a bool
                2 | def submit(finalize: bool = NO):
                  |                             ^^
                the default, `0`, is made at lib.star:1:6"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(d: dict[str, int] = {"a": "b"}):
    pass"#,
        &expect![[r#"
            lib.star:1:27: a value of `d`'s default can't be `"b"`: give an int
                1 | def f(d: dict[str, int] = {"a": "b"}):
                  |                           ^^^^^^^^^^
                the value, `"b"`, is made at lib.star:1:33"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(x: Strng):
    pass"#,
        &expect![[r#"
            lib.star:1:10: `Strng` is no type: give `None`, `bool`, `int`, `float`, `str`, `list[…]`, `dict[…, …]`, `Target`, `Runner`, `Run` or `Payload`
                1 | def f(x: Strng):
                  |          ^^^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(x: "str"):
    pass"#,
        &expect![[r#"
            lib.star:1:10: unexpected string: expected a type, as `str` or `list[int]`
                1 | def f(x: "str"):
                  |          ^^^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(xs: list):
    pass"#,
        &expect![[r#"
            lib.star:1:11: `list` takes its elements' type: write `list[str]`
                1 | def f(xs: list):
                  |           ^^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f() -> dict:
    pass"#,
        &expect![[r#"
            lib.star:1:12: `dict` takes its keys' and values' types: write `dict[str, int]`
                1 | def f() -> dict:
                  |            ^^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(d: dict[str]):
    pass"#,
        &expect![[r#"
            lib.star:1:18: unexpected `]`: expected `,` and the values' type
                1 | def f(d: dict[str]):
                  |                  ^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(d: dict[list[str], int]):
    pass"#,
        &expect![[r#"
            lib.star:1:15: a dict's keys can't be lists of strings: key a dict with strings or numbers, as `str`, `int` or `float`
                1 | def f(d: dict[list[str], int]):
                  |               ^^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f(x: int | None = None):
    pass"#,
        &expect![[r#"
            lib.star:1:14: `|` in a type is not in Pumpkin's Starlark: take one type
                1 | def f(x: int | None = None):
                  |              ^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f() - > int:
    pass"#,
        &expect![[r#"
            lib.star:1:9: unexpected `-`: expected `:`
                1 | def f() - > int:
                  |         ^"#]],
    );
}

#[test]
fn picks() {
    // The agent's image alone is picked, and is given once the images it is built on are built.
    let files = files();
    let shown = on_stack(|| {
        let (_, case) = Case::parse(IMAGES, &files, Bounds::WORKFLOW);
        let mut sources = Sources {
            held: &case.held,
            files: case.files,
        };
        let entry = Label::parse(&format!("//{}", case.entry)).unwrap();
        let args = Args::parse([]).unwrap();
        let plan = lang::evaluate(&entry, &mut sources, &args, Bounds::WORKFLOW).unwrap();
        let mut built = Vec::new();
        let mut started = 0;
        let step = |ready: Vec<(usize, Declared<'_, Built>)>| {
            let ready = ready.into_iter().map(|(index, declared)| {
                started += 1;
                (index, stand_in(declared, started, &mut built))
            });
            ready.collect()
        };
        let picked = plan.build_for(
            JOBS,
            |recipe| recipe.recipe.as_str() == "agent/Dockerfile",
            step,
            |picked| {
                let picked = picked.iter().map(|Declared { recipe, contexts }| {
                    let mut line = format!("picked: {}'s {}", recipe.context, recipe.recipe);
                    for (name, built) in contexts {
                        let _ = write!(line, ", with image {} as {name}", built.number);
                    }
                    line
                });
                picked.collect::<Vec<_>>()
            },
        );
        built.extend(picked.unwrap());
        built.join("\n")
    });
    expect![[r#"
        image 1: //tasks/images's tools/Dockerfile, with BASE="docker.io/example/base@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", in pumpkin/tasks/images/tools
        image 2: //tasks/images's toolchain/Dockerfile, its stage go, in pumpkin/tasks/images/toolchain
        picked: //tasks/images's agent/Dockerfile, with image 1 as build-tools, with image 2 as toolchain"#]].assert_eq(&shown);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshots"
)]
fn plans() {
    check(
        &format!(
            "
#- --arg model=model-large
#- --arg deadline=+7200
#- --arg strict=true
{IMAGES}"
        ),
        &expect![[r#"
            image 1: //tasks/images's tools/Dockerfile, with BASE="docker.io/example/base@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", in pumpkin/tasks/images/tools
            image 2: //tasks/images's toolchain/Dockerfile, its stage go, in pumpkin/tasks/images/toolchain
            building 1 and 2
            image 3: //tasks/images's judge/Dockerfile, with //shared at shared, its stage train, with BASE="docker.io/example/base@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", in pumpkin/tasks/images/judge
            building 2 and 3
            image 4: //tasks/images's agent/Dockerfile, with //shared at shared, with image 1 as build-tools, with image 2 as toolchain, in pumpkin/tasks/images/agent
            building 3 and 4
            image 5: //tasks/images's judge/Dockerfile, with //shared at shared, its stage eval, with BASE="docker.io/example/base@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", in pumpkin/tasks/images/judge
            building 4 and 5
            image 6: //harness's Dockerfile, with image 4 as base, in pumpkin/harness
            building 5 and 6
            building 6
            {
              "deadline_seconds": 7200,
              "final": {
                "body": {
                  "def": "final_call",
                  "file": "//lib/images.star"
                },
                "payload_max_bytes": 65536,
                "target": "final_judge",
                "timeout_seconds": 120
              },
              "ir_version": "pumpkin.ir/v1",
              "name": "neutral/images",
              "roles": [
                {
                  "image": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb6",
                  "name": "agent",
                  "needs": {
                    "cpu_millis": 4000,
                    "memory_bytes": 8589934592
                  },
                  "runtime": {
                    "agent": {
                      "command": {
                        "argv": [
                          "/harness/bin/agent"
                        ],
                        "cwd": "/workspace"
                      },
                      "egress": {
                        "model": [
                          {
                            "api": "responses",
                            "models": [
                              "model-large"
                            ]
                          }
                        ]
                      },
                      "finalize_grace_seconds": 0,
                      "grants": [
                        {
                          "job": {
                            "body": {
                              "def": "train",
                              "file": "//lib/images.star"
                            },
                            "call_timeout_seconds": 600,
                            "calls": 10,
                            "payload_max_bytes": 65536,
                            "target": "trainer"
                          },
                          "name": "train"
                        },
                        {
                          "name": "final",
                          "now": {
                            "body": {
                              "arguments": {
                                "finalize": {
                                  "default": false,
                                  "type": "bool"
                                }
                              },
                              "def": "submit",
                              "file": "//lib/submit.star"
                            },
                            "payload_max_bytes": 65536
                          }
                        }
                      ],
                      "stdin": "Solve the task in /task/README.md, then submit with \"sandboxctl\".\n"
                    }
                  },
                  "user": {
                    "gid": 1000,
                    "uid": 1000
                  }
                },
                {
                  "image": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb3",
                  "name": "trainer",
                  "needs": {
                    "cpu_millis": 1000,
                    "memory_bytes": 536870912
                  },
                  "runtime": {
                    "target": {
                      "cwd": "/pumpkin/home"
                    }
                  },
                  "user": {
                    "gid": 65532,
                    "uid": 65532
                  }
                },
                {
                  "image": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb5",
                  "name": "final_judge",
                  "needs": {
                    "cpu_millis": 1000,
                    "memory_bytes": 536870912
                  },
                  "runtime": {
                    "target": {
                      "cwd": "/pumpkin/home"
                    }
                  },
                  "user": {
                    "gid": 65532,
                    "uid": 65532
                  }
                }
              ]
            }"#]],
    );
    check(
        &format!(
            "
#- --arg deadline=1h
#- --arg mdoel=model-large
{IMAGES}"
        ),
        &expect![[r#"
            lib/images.star:10:12: `--arg deadline` is `"1h"`, which `int` can't read: give it decimal digits, after `-` or `+` or neither, from -2^63 to 2^63 - 1
                10 | DEADLINE = arg("deadline", int, default = 5 * 3600)
                   |            ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                `--arg mdoel` is given, and no `arg()` call before this one declares it"#]],
    );
    check(
        &format!(
            "
#- --arg strict=yes
{IMAGES}"
        ),
        &expect![[r#"
            lib/images.star:11:10: `--arg strict` is `"yes"`, which is no bool: give it `true` or `false`
                11 | STRICT = arg("strict", bool, default = False)
                   |          ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        &format!(
            "
#- --arg mdoel=model-large
#- --arg model=model-large
#- --arg efort=low
{IMAGES}"
        ),
        &expect![[r#"
            tasks/images/task.star: `--arg efort` is declared by no `arg()` call: give one of `deadline`, `model`, `strict`
            tasks/images/task.star: `--arg mdoel` is declared by no `arg()` call: give one of `deadline`, `model`, `strict`"#]],
    );
    check(
        &format!(
            "
#- --arg model
#- --arg =model-large
#- --arg model=a
#- --arg model=b
{IMAGES}"
        ),
        &expect![[r#"
            `--arg model` has no `=`: give it as `--arg <name>=<value>`
            `--arg =model-large` names no arg before its `=`: give it as `--arg <name>=<value>`
            `--arg model=b` names an arg given already: give each arg once"#]],
    );
    check(
        "
#- --arg model=model-large
#- //tasks/literal/task.star",
        &expect![
            "tasks/literal/task.star: `--arg model` is declared by no `arg()` call, as none is made: drop it"
        ],
    );
    check(
        r#"
#- //bad/refused/task.star
"""A task whose recipes the tests' check refuses, and whose agent's needs the IR refuses."""

load("//lib/images.star", "images_task")

images_task(agent = {"cpu_millis": 0, "memory_bytes": 8 * GiB})
"#,
        &expect![[r#"
            lib/images.star:56:13: the stand-in check refuses `tools/Dockerfile`
                56 |     tools = image("tools/Dockerfile", args = BASE_ARGS)
                   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `images_task()`, called at bad/refused/task.star:5:1
            lib/images.star:57:17: the stand-in check refuses `toolchain/Dockerfile`
                57 |     toolchain = image("toolchain/Dockerfile", target = "Go")
                   |                 ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `images_task()`, called at bad/refused/task.star:5:1
            lib/images.star:59:19: the stand-in check refuses `agent/Dockerfile`, given build-tools, given toolchain
                59 |     agent_image = image("agent/Dockerfile", context = ".", mount = SHARED, contexts = built_on)
                   |                   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `images_task()`, called at bad/refused/task.star:5:1
            lib/images.star:59:19: the stand-in check refuses the mount at `shared`
                59 |     agent_image = image("agent/Dockerfile", context = ".", mount = SHARED, contexts = built_on)
                   |                   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `images_task()`, called at bad/refused/task.star:5:1
            lib/images.star:25:18: the stand-in check refuses `judge/Dockerfile`
                25 |         "image": image("judge/Dockerfile", mount = SHARED, target = stage, args = BASE_ARGS),
                   |                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `_role()`, called at lib/images.star:63:9
                in `images_task()`, called at bad/refused/task.star:5:1
            lib/images.star:25:18: the stand-in check refuses the mount at `shared`
                25 |         "image": image("judge/Dockerfile", mount = SHARED, target = stage, args = BASE_ARGS),
                   |                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `_role()`, called at lib/images.star:63:9
                in `images_task()`, called at bad/refused/task.star:5:1
            lib/images.star:25:18: the stand-in check refuses `judge/Dockerfile`
                25 |         "image": image("judge/Dockerfile", mount = SHARED, target = stage, args = BASE_ARGS),
                   |                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `_role()`, called at lib/images.star:64:9
                in `images_task()`, called at bad/refused/task.star:5:1
            lib/images.star:25:18: the stand-in check refuses the mount at `shared`
                25 |         "image": image("judge/Dockerfile", mount = SHARED, target = stage, args = BASE_ARGS),
                   |                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `_role()`, called at lib/images.star:64:9
                in `images_task()`, called at bad/refused/task.star:5:1
            bad/refused/task.star:5:36: `cpu_millis` can't be `0`: expected a positive int
                5 | images_task(agent = {"cpu_millis": 0, "memory_bytes": 8 * GiB})
                  |                                    ^
                the IR's /roles/0/needs/cpu_millis"#]],
    );
    check(
        r#"
#- //bad/broken/task.star
"""A task whose images the tests' build fails to build, the first two at once."""

load("//lib/images.star", "images_task")

images_task(agent = {"cpu_millis": 4000, "memory_bytes": 8 * GiB})
"#,
        &expect![[r#"
            lib/images.star:56:13: building the image failed: the stand-in build fails `tools/Dockerfile`
                56 |     tools = image("tools/Dockerfile", args = BASE_ARGS)
                   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `images_task()`, called at bad/broken/task.star:5:1
            lib/images.star:57:17: building the image failed: the stand-in build fails `toolchain/Dockerfile`
                57 |     toolchain = image("toolchain/Dockerfile", target = "Go")
                   |                 ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `images_task()`, called at bad/broken/task.star:5:1"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshots"
)]
fn builtins() {
    check(
        r#"fail("[" + package_name() + "]")"#,
        &expect![[r#"
            text.star:1:1: fail: []
                1 | fail("[" + package_name() + "]")
                  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"fail("{} {} {}".format(MODEL_URL, MAX_MODEL_REQUESTS - 1, 20 * USD))"#,
        &expect![[r#"
            text.star:1:1: fail: http://127.0.0.1:11235 6 20000000
                1 | fail("{} {} {}".format(MODEL_URL, MAX_MODEL_REQUESTS - 1, 20 * USD))
                  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"
A = image("Dockerfile", context = "//images")
B = image("Dockerfile", context = "//images", mount = {})
C = image("x/Dockerfile", context = "//images")
fail("A == B: {}, A == C: {}".format(int(A == B), int(A == C)))"#,
        &expect![[r#"
            text.star:4:1: fail: A == B: 1, A == C: 0
                4 | fail("A == B: {}, A == C: {}".format(int(A == B), int(A == C)))
                  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"
I = "//images"
TRAIN, ARG1, ARG2 = image("Dockerfile", context = I, target = "train"), image("Dockerfile", context = I, args = {"N": "1"}), image("Dockerfile", context = I, args = {"N": "2"})
same = [
    TRAIN == image("Dockerfile", context = I, target = "train", args = {}),
    TRAIN == image("Dockerfile", context = I, target = "Train"),
    image("Dockerfile", context = I) == image("Dockerfile", context = I, target = None, contexts = {}),
    image("Dockerfile", context = I, contexts = {"a": ARG1, "b": ARG2}) == image("Dockerfile", context = I, contexts = {"b": ARG2, "a": ARG1}),
]
other = [
    TRAIN == image("Dockerfile", context = I, target = "eval"),
    ARG1 == ARG2,
    image("Dockerfile", context = I, contexts = {"a": ARG1}) == image("Dockerfile", context = I, contexts = {"a": ARG2}),
    image("Dockerfile", context = I, contexts = {"a": ARG1}) == image("Dockerfile", context = I, contexts = {"b": ARG1}),
]
fail("same: {}, other: {}".format(
    " ".join([str(int(each)) for each in same]),
    " ".join([str(int(each)) for each in other]),
))"#,
        &expect![[r#"
            text.star:15:1: fail: same: 1 1 1 1, other: 0 0 0 0
                15 | fail("same: {}, other: {}".format(
                   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = image("Dockerfile", context = "//images", target = "train!")"#,
        &expect![[r#"
            text.star:1:56: `train!` is no stage's name: use a-z, 0-9, `-`, `_` and `.`, starting with a letter
                1 | X = image("Dockerfile", context = "//images", target = "train!")
                  |                                                        ^^^^^^^^"#]],
    );
    check(
        r#"X = image("Dockerfile", context = "//images", target = 1)"#,
        &expect![[r#"
            text.star:1:56: `image`'s `target` can't be `1`: give a string
                1 | X = image("Dockerfile", context = "//images", target = 1)
                  |                                                        ^"#]],
    );
    check(
        r#"X = image("Dockerfile", context = "//images", args = ["N=1"])"#,
        &expect![[r#"
            text.star:1:54: `image`'s `args` can't be a list: give a dict of strings, each by its build argument's name
                1 | X = image("Dockerfile", context = "//images", args = ["N=1"])
                  |                                                      ^^^^^^^"#]],
    );
    check(
        r#"X = image("Dockerfile", context = "//images", args = {1: "1"})"#,
        &expect![[r#"
            text.star:1:55: `image`'s `args` can't key a value by `1`: key it by its build argument's name, a string
                1 | X = image("Dockerfile", context = "//images", args = {1: "1"})
                  |                                                       ^"#]],
    );
    check(
        r#"X = image("Dockerfile", context = "//images", args = {"N": 1})"#,
        &expect![[r#"
            text.star:1:60: a value of `image`'s `args` can't be `1`: give a string
                1 | X = image("Dockerfile", context = "//images", args = {"N": 1})
                  |                                                            ^"#]],
    );
    check(
        r#"X = image("Dockerfile", context = "//images", contexts = ["Dockerfile"])"#,
        &expect![[r#"
            text.star:1:58: `image`'s `contexts` can't be a list: give a dict of images, each by the name the recipe gives it
                1 | X = image("Dockerfile", context = "//images", contexts = ["Dockerfile"])
                  |                                                          ^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = image("Dockerfile", context = "//images", contexts = {1: image("Dockerfile", context = "//images")})"#,
        &expect![[r#"
            text.star:1:59: `image`'s `contexts` can't key a value by `1`: key it by the name the recipe gives it, a string
                1 | X = image("Dockerfile", context = "//images", contexts = {1: image("Dockerfile", context = "//images")})
                  |                                                           ^"#]],
    );
    check(
        r#"X = image("Dockerfile", context = "//images", contexts = {"base": "Dockerfile"})"#,
        &expect![[r#"
            text.star:1:67: a value of `image`'s `contexts` can't be `"Dockerfile"`: give an image, as `image()` makes
                1 | X = image("Dockerfile", context = "//images", contexts = {"base": "Dockerfile"})
                  |                                                                   ^^^^^^^^^^^^"#]],
    );
    check(
        r#"
X = arg(name = "n", type = str, default = "x")
fail(X)"#,
        &expect![[r#"
            text.star:2:1: fail: x
                2 | fail(X)
                  | ^^^^^^^"#]],
    );
    check(
        r#"
#- --arg a=x
#- --arg m=1
A = arg("a", str, default = "")
N = arg("n", int)"#,
        &expect![[r#"
            text.star:2:5: `--arg n` is required, as this `arg()` gives it no default: give it as `--arg n=<value>`
                2 | N = arg("n", int)
                  |     ^^^^^^^^^^^^^
                `--arg m` is given, and no `arg()` call before this one declares it"#]],
    );
    check(
        r#"
A = arg("n", str, default = "")
B = arg("n", int, default = 0)"#,
        &expect![[r#"
            text.star:2:5: `--arg n` is declared already: declare each arg once
                2 | B = arg("n", int, default = 0)
                  |     ^^^^^^^^^^^^^^^^^^^^^^^^^^
                it is first declared at text.star:1:5"#]],
    );
    check(
        r#"X = arg("n", "int")"#,
        &expect![[r#"
            text.star:1:14: `arg`'s `type` can't be `"int"`: give the builtin `str`, `int` or `bool`
                1 | X = arg("n", "int")
                  |              ^^^^^"#]],
    );
    check(
        r#"X = arg("n", float)"#,
        &expect![[r#"
        text.star:1:14: an arg can't be a float: give `arg`'s `type` as the builtin `str`, `int` or `bool`
            1 | X = arg("n", float)
              |              ^^^^^"#]],
    );
    check(
        r#"X = arg("a b", str)"#,
        &expect![[r#"
            text.star:1:9: `a b` is no arg's name: name it as Starlark names a value, with ASCII letters, digits and `_`, not starting with a digit
                1 | X = arg("a b", str)
                  |         ^^^^^"#]],
    );
    check(
        r#"X = arg("n", int, default = "1")"#,
        &expect![[r#"
            text.star:1:29: an arg of type `int` can't default to `"1"`: give an int or `None`
                1 | X = arg("n", int, default = "1")
                  |                             ^^^"#]],
    );
    check(
        r#"X = arg("n", bool, default = 1)"#,
        &expect![[r#"
            text.star:1:30: an arg of type `bool` can't default to `1`: give a bool or `None`
                1 | X = arg("n", bool, default = 1)
                  |                              ^"#]],
    );
    check(
        r#"
#- --arg n=5
#- --arg s=None
D = arg("d", int, default = None)
N = arg("n", int, default = None)
S = arg("s", str, default = None)
T = arg("t", str, default = None)
B = arg("b", bool, default = None)
DEADLINE = 3600 if D == None else D
fail(", ".join([str(N + 1), S, "D is None" if D == None else "", "T is None" if T == None else "", "B is None" if B == None else "", str(DEADLINE)]))"#,
        &expect![[r#"
            text.star:7:1: fail: 6, None, D is None, T is None, B is None, 3600
                7 | fail(", ".join([str(N + 1), S, "D is None" if D == None else "", "T is None" if T == None else "", "B is None" if B == None else "", str(DEADLINE)]))
                  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = arg("n", str, "x")"#,
        &expect![[r#"
            text.star:1:19: `arg` takes 2 arguments by position: name the rest
                1 | X = arg("n", str, "x")
                  |                   ^^^"#]],
    );
    check(
        r#"X = arg("n")"#,
        &expect![[r#"
            text.star:1:5: `arg` lacks `type`: give it
                1 | X = arg("n")
                  |     ^^^^^^^^"#]],
    );
    check(
        "X = arg(1, str)",
        &expect![[r#"
            text.star:1:9: `arg`'s `name` can't be `1`: give a string
                1 | X = arg(1, str)
                  |         ^"#]],
    );
    check(
        r#"
#- --arg n=given
#- --arg m=1
#- //text.star
load("//lib.star", "X")
#- //lib.star
def named(task):
    N = arg("n", str, default = None)
    return task if N == None else N
def counted():
    return arg("m", int, default = None)
X = named("t")"#,
        &expect![[r#"
            text.star: this file calls no `workflow()`: call it once, with the workflow
            text.star: `--arg m` is declared by no `arg()` call: give one of `n`"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
X = package_name()"#,
        &expect![[r#"
            lib.star:1:5: `package_name()` is called while `//lib.star` loads: call it in the entry file, which loads `//lib.star`
                1 | X = package_name()
                  |     ^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
X = image("Dockerfile", context = "//images")"#,
        &expect![[r#"
            lib.star:1:5: `image()` is called while `//lib.star` loads: call it in the entry file, which loads `//lib.star`
                1 | X = image("Dockerfile", context = "//images")
                  |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        "X = package_name(1)",
        &expect![[r#"
            text.star:1:18: `package_name` takes no argument: drop it
                1 | X = package_name(1)
                  |                  ^"#]],
    );
    check(
        r#"X = image("Dockerfile", contxt = "//images")"#,
        &expect![[r#"
            text.star:1:25: `contxt` is no parameter of `image`: expected one of `recipe`, `context`, `mount`, `target`, `args`, `contexts`
                1 | X = image("Dockerfile", contxt = "//images")
                  |                         ^^^^^^"#]],
    );
    check(
        r#"X = image("../Dockerfile", context = "//images")"#,
        &expect![[r#"
            text.star:1:11: `../Dockerfile` names no path in the context: write `/`-separated components of ASCII letters, digits, `.`, `_` and `-`, none of them empty, `.` or `..`
                1 | X = image("../Dockerfile", context = "//images")
                  |           ^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = image("//images/Dockerfile", context = "//images")"#,
        &expect![[r#"
            text.star:1:11: `//images/Dockerfile` names no path in the context: write `/`-separated components of ASCII letters, digits, `.`, `_` and `-`, none of them empty, `.` or `..`
                1 | X = image("//images/Dockerfile", context = "//images")
                  |           ^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = image("Dockerfile", context = "..")"#,
        &expect![[r#"
            text.star:1:35: `..` names no directory below the root: write `.` for the entry file's directory, a path below it, or `//` and a path below the root, each in `/`-separated components of ASCII letters, digits, `.`, `_` and `-`, none of them empty, `.` or `..`
                1 | X = image("Dockerfile", context = "..")
                  |                                   ^^^^"#]],
    );
    check(
        r#"X = image("Dockerfile")"#,
        &expect![[r#"
            text.star:1:5: the context defaults to the entry file's directory, the root, which can't be a context: give `context` a directory below it
                1 | X = image("Dockerfile")
                  |     ^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = image("Dockerfile", context = "//Images")"#,
        &expect![[r#"
            text.star:1:35: `Images`, a directory that holds the recipe, can't name a part of the image's Docker repository, which the recipe's directory names: name it with lowercase letters and digits, in runs joined by one `.`, one or two `_`, or dashes
                1 | X = image("Dockerfile", context = "//Images")
                  |                                   ^^^^^^^^^^"#]],
    );
    check(
        r#"X = image("Agent/Dockerfile", context = "//images")"#,
        &expect![[r#"
            text.star:1:11: `Agent`, a directory that holds the recipe, can't name a part of the image's Docker repository, which the recipe's directory names: name it with lowercase letters and digits, in runs joined by one `.`, one or two `_`, or dashes
                1 | X = image("Agent/Dockerfile", context = "//images")
                  |           ^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = image("agent-/Dockerfile", context = "//images")"#,
        &expect![[r#"
            text.star:1:11: `agent-`, a directory that holds the recipe, can't name a part of the image's Docker repository, which the recipe's directory names: name it with lowercase letters and digits, in runs joined by one `.`, one or two `_`, or dashes
                1 | X = image("agent-/Dockerfile", context = "//images")
                  |           ^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = image("Dockerfile", context = "//images/a__b/a___b")"#,
        &expect![[r#"
            text.star:1:35: `a___b`, a directory that holds the recipe, can't name a part of the image's Docker repository, which the recipe's directory names: name it with lowercase letters and digits, in runs joined by one `.`, one or two `_`, or dashes
                1 | X = image("Dockerfile", context = "//images/a__b/a___b")
                  |                                   ^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = image("Dockerfile", context = "//images", mount = {"shared": "//"})"#,
        &expect![[r#"
            text.star:1:66: `//` names no directory below the root: write `.` for the entry file's directory, a path below it, or `//` and a path below the root, each in `/`-separated components of ASCII letters, digits, `.`, `_` and `-`, none of them empty, `.` or `..`
                1 | X = image("Dockerfile", context = "//images", mount = {"shared": "//"})
                  |                                                                  ^^^^"#]],
    );
    check(
        r#"X = image("Dockerfile", context = "//images", mount = {1: "//shared"})"#,
        &expect![[r#"
            text.star:1:56: `image`'s `mount` can't key a value by `1`: key it by its path in the context, a string
                1 | X = image("Dockerfile", context = "//images", mount = {1: "//shared"})
                  |                                                        ^"#]],
    );
    check(
        r#"X = image("Dockerfile", context = "//images", mount = ["//shared"])"#,
        &expect![[r#"
            text.star:1:55: `image`'s `mount` can't be a list: give a dict of directories, each by its path in the context
                1 | X = image("Dockerfile", context = "//images", mount = ["//shared"])
                  |                                                       ^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = image("Dockerfile", context = "//images", mount = {"shared": 1})"#,
        &expect![[r#"
            text.star:1:66: a directory of `image`'s `mount` can't be `1`: give a string
                1 | X = image("Dockerfile", context = "//images", mount = {"shared": 1})
                  |                                                                  ^"#]],
    );
    check(
        r#"X = image("Dockerfile", context = "//images", mount = {"a/../b": "//shared"})"#,
        &expect![[r#"
            text.star:1:56: `a/../b` names no path in the context: write `/`-separated components of ASCII letters, digits, `.`, `_` and `-`, none of them empty, `.` or `..`
                1 | X = image("Dockerfile", context = "//images", mount = {"a/../b": "//shared"})
                  |                                                        ^^^^^^^^"#]],
    );
    check(
        r#"
X = image("abcde/Dockerfile", context = "//" + "/".join(["abcdefghij" for a in [1, 2] for b in [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]]))
fail("255 characters fit")"#,
        &expect![[r#"
            text.star:2:1: fail: 255 characters fit
                2 | fail("255 characters fit")
                  | ^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = image("Dockerfile", context = "//" + "/".join(["abcdefghij" for a in [1, 2, 3, 4, 5] for b in [1, 2, 3, 4, 5]]))"#,
        &expect![[r#"
            text.star:1:5: the image's repository, `pumpkin/abcdefghij/abcdefghij/abcdefghij/abcdefghij/abcdefgh…`, has 282 characters, more than Docker's 255: name the directory of the recipe with fewer
                1 | X = image("Dockerfile", context = "//" + "/".join(["abcdefghij" for a in [1, 2, 3, 4, 5] for b in [1, 2, 3, 4, 5]]))
                  |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        "X = image(1)",
        &expect![[r#"
            text.star:1:11: `image`'s `recipe` can't be `1`: give a string
                1 | X = image(1)
                  |           ^"#]],
    );
    check(
        r#"
workflow(name = "neutral/twice", roles = [], final = None, deadline_seconds = 3600)
workflow(name = "neutral/twice", roles = [], final = None, deadline_seconds = 3600)"#,
        &expect![[r#"
            text.star:2:1: `workflow()` is called a second time: call it once
                2 | workflow(name = "neutral/twice", roles = [], final = None, deadline_seconds = 3600)
                  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                it is first called at text.star:1:1"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshots"
)]
fn json() {
    // Keys sorted by their bytes; `"`, `\`, what is below U+0020 and U+007F escaped, U+0085 not.
    check(
        r#"
X = json.encode([None, True, False, -123, (1, [2]), {"b": 1, "a": {}, "B": [], "é": "", "": 0}])
Y = json.encode("q\"b\\s/\n\t\r\b\f\001\037\177'")
Z = json.decode('"\\u0085\\u009fé😹"')
fail(" ".join([X, Y, str(int(json.encode(Z) == '"' + Z + '"'))]))"#,
        &expect![[r#"
            text.star:4:1: fail: [null,true,false,-123,[1,[2]],{"":0,"B":[],"a":{},"b":1,"é":""}] "q\"b\\s/\n\t\r\b\f\u0001\u001f\u007f'" 1
                4 | fail(" ".join([X, Y, str(int(json.encode(Z) == '"' + Z + '"'))]))
                  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = json.encode({"a": [float("-inf")]})"#,
        &expect![[r#"
            text.star:1:17: `json.encode` can't encode `-inf`, as JSON has no NaN or infinity: give it finite floats
                1 | X = json.encode({"a": [float("-inf")]})
                  |                 ^^^^^^^^^^^^^^^^^^^^^^
                the value, `-inf`, is made at text.star:1:24"#]],
    );
    // An object's keys in the order the text gives them, as Starlark's `dict` keeps them.
    check(
        r#"
D = json.decode(' {"b": [1, -0, null, true, false, -9223372036854775808], "a": {"c": "\\u00e9\\ud83d\\ude39\\/\\"\\\\"}, "z": []}\n')
fail(" ".join([",".join(D.keys()), json.encode(D["b"]), D["a"]["c"], str(int(D["b"][2] == None))]))"#,
        &expect![[r#"
            text.star:2:1: fail: b,a,z [1,0,null,true,false,-9223372036854775808] é😹/"\ 1
                2 | fail(" ".join([",".join(D.keys()), json.encode(D["b"]), D["a"]["c"], str(int(D["b"][2] == None))]))
                  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"
V = {"k": [1, "a\n", {"x": None}, True], "": -5}
fail(str(int(json.decode(json.encode(V)) == V)) + str(int(json.decode("[]") == [])))"#,
        &expect![[r#"
            text.star:2:1: fail: 11
                2 | fail(str(int(json.decode(json.encode(V)) == V)) + str(int(json.decode("[]") == [])))
                  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    // The cases of Bazel's and starlark-go's `json.star` that hold nothing the subset leaves out
    // and no other case takes, but Bazel's `1e999` and `-1e999`, which starlark-rust refuses too.
    check(
        r#"
X = [
    json.decode("\t[\t1,\r2,\n3]\n") == [1, 2, 3],
    json.decode('\n{\t"a":\r1\t}\n') == {"a": 1},
    json.decode('{"foo\\u0000bar": 42}') == {"foo\000bar": 42},
    json.decode('"\\u0123"') == "ģ",
    json.decode('"\\ud83d\\ude39\\ud83d\\udc8d"') == "😹💍",
    json.decode('"\177"') == "\177",
    json.decode('"\\b\\f\\n\\r\\t\\/"') == "\b\f\n\r\t/",
    json.decode("[1]") == [1],
    json.decode('{"one": 1, "two": 2}') == {"one": 1, "two": 2},
    json.encode("hello") == '"hello"',
    json.encode((1, 2, 3)) == "[1,2,3]",
    json.encode(["'", "/", "", "¼Δ", "😹"]) == '["\'","/","","¼Δ","😹"]',
    json.decode(json.encode(["😿", "🐱‍👤"])) == ["😿", "🐱‍👤"],
    json.encode(12.345e67) == "1.2345e+68",
    json.decode("3.539537889086625e+24") == 3.539537889086625e+24,
    json.decode("0e+1") == 0,
    str(json.decode("-0.0")) == "-0.0",
    json.decode("-0.000000000000000000000000000000000000000000000000000000000000000000000000000001") == -0.000000000000000000000000000000000000000000000000000000000000000000000000000001,
    json.decode("5e-1") == 0.5,
    json.decode("5e1") == 50.0,
    json.decode("5.0e1") == 50.0,
    str(json.decode(json.encode(1.0))) == "1.0",
    json.decode(json.encode(-1.23e-45)) == -1.23e-45,
]
fail("".join([str(int(x)) for x in X]))"#,
        &expect![[r#"
            text.star:26:1: fail: 11111111111111111111111
                26 | fail("".join([str(int(x)) for x in X]))
                   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    let bytes = |bytes| Bounds {
        bytes,
        ..Bounds::WORKFLOW
    };
    // Each is refused one byte fewer: 160 of the list's elements and 11 of the text, `["ab","cd"]`;
    // 2 of the string and 80 of the list's element.
    let encoded = "X = json.encode([\"ab\", \"cd\"])\nfail(X)";
    check_within(
        bytes(171),
        encoded,
        &expect![[r#"
        text.star:2:1: fail: ["ab","cd"]
            2 | fail(X)
              | ^^^^^^^"#]],
    );
    check_within(
        bytes(170),
        encoded,
        &expect![[r#"
        text.star:1:17: this evaluation's strings, lists, tuples and dicts take more than 170 bytes: make smaller values, as with fewer `+` of long ones
            1 | X = json.encode(["ab", "cd"])
              |                 ^^^^^^^^^^^^"#]],
    );
    let decoded = "X = json.decode('[\"ab\"]')\nfail(X[0])";
    check_within(
        bytes(82),
        decoded,
        &expect![[r#"
        text.star:2:1: fail: ab
            2 | fail(X[0])
              | ^^^^^^^^^^"#]],
    );
    check_within(
        bytes(81),
        decoded,
        &expect![[r#"
        text.star:1:5: this evaluation's strings, lists, tuples and dicts take more than 81 bytes: make smaller values, as with fewer `+` of long ones
            1 | X = json.decode('["ab"]')
              |     ^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    let deep = |depth: usize| {
        format!(
            "T = \"{}{}\"\nX = json.decode(T)\nfail(json.encode(X))",
            "[".repeat(depth),
            "]".repeat(depth)
        )
    };
    check(
        &deep(128),
        &expect![[r#"
        text.star:3:1: fail: [[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]
            3 | fail(json.encode(X))
              | ^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        &deep(129),
        &expect![[r#"
        text.star:2:17: `json.decode`'s text nests arrays and objects 129 deep at offset 128: nest them at most 128 deep
            2 | X = json.decode(T)
              |                 ^
            the text, `"[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[…"`, is made at text.star:1:5"#]],
    );
    // A closed array no longer counts towards how deep the next one nests.
    let siblings = format!(
        "X = json.decode(\"[{}[]]\")\nfail(str(len(X)))",
        "[],".repeat(128)
    );
    check(
        &siblings,
        &expect![[r#"
        text.star:2:1: fail: 129
            2 | fail(str(len(X)))
              | ^^^^^^^^^^^^^^^^^"#]],
    );
    // Each element is refused as it would pass the budget, before the text's error is reached.
    let elements = "X = json.decode('[1, 1, 1')";
    check_within(
        bytes(200),
        elements,
        &expect![[r#"
        text.star:1:5: this evaluation's strings, lists, tuples and dicts take more than 200 bytes: make smaller values, as with fewer `+` of long ones
            1 | X = json.decode('[1, 1, 1')
              |     ^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    // The elements of all the arrays open count together: these, 128 deep and 4 each, take 40960
    // bytes, while each array alone takes 320.
    let open = r#"
A = "[0, 0, 0, 0, "
B = A + A
C = B + B
D = C + C
E = D + D
F = E + E
G = F + F
H = G + G
X = json.decode(H)"#;
    check_within(
        bytes(20_000),
        open,
        &expect![[r#"
        text.star:9:5: this evaluation's strings, lists, tuples and dicts take more than 20000 bytes: make smaller values, as with fewer `+` of long ones
            9 | X = json.decode(H)
              |     ^^^^^^^^^^^^^^"#]],
    );
    let entries = r#"X = json.decode('{"a": 1, "b": 2, "c": 3')"#;
    check_within(
        bytes(400),
        entries,
        &expect![[r#"
        text.star:1:5: this evaluation's strings, lists, tuples and dicts take more than 400 bytes: make smaller values, as with fewer `+` of long ones
            1 | X = json.decode('{"a": 1, "b": 2, "c": 3')
              |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    // A closed array or object no longer counts as pending: each passes at its exact bound, 321
    // and 483 bytes, where counting a closed one's elements again would refuse it.
    let closed = "X = json.decode('[[1], [2]]')\nfail(str(len(X)))";
    check_within(
        bytes(321),
        closed,
        &expect![[r#"
        text.star:2:1: fail: 2
            2 | fail(str(len(X)))
              | ^^^^^^^^^^^^^^^^^"#]],
    );
    let closed = "X = json.decode('[{\"a\": 1}, {\"b\": 2}]')\nfail(str(len(X)))";
    check_within(
        bytes(483),
        closed,
        &expect![[r#"
        text.star:2:1: fail: 2
            2 | fail(str(len(X)))
              | ^^^^^^^^^^^^^^^^^"#]],
    );
    check_within(
        bytes(482),
        closed,
        &expect![[r#"
        text.star:2:6: this evaluation's strings, lists, tuples and dicts take more than 482 bytes: make smaller values, as with fewer `+` of long ones
            2 | fail(str(len(X)))
              |      ^^^^^^^^^^^"#]],
    );
    // A string's escaped text is asked of the budget before it is written: this key's takes 6146
    // bytes, and the value's refusal follows it.
    let escaped = r#"
A = "\001\001\001\001\001\001\001\001"
B = A + A
C = B + B
D = C + C
E = D + D
F = E + E
G = F + F
H = G + G
X = json.encode({H: len})"#;
    check_within(
        bytes(5000),
        escaped,
        &expect![[r#"
        text.star:9:17: this evaluation's strings, lists, tuples and dicts take more than 5000 bytes: make smaller values, as with fewer `+` of long ones
            9 | X = json.encode({H: len})
              |                 ^^^^^^^^"#]],
    );
    check(
        "fail(str(int(json == json)))",
        &expect![[r#"
        text.star:1:1: fail: 1
            1 | fail(str(int(json == json)))
              | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = json.encode({1: "a"})"#,
        &expect![[r#"
        text.star:1:17: `json.encode` can't encode a dict keyed by `1`: key each dict it encodes with strings
            1 | X = json.encode({1: "a"})
              |                 ^^^^^^^^
            the key, `1`, is made at text.star:1:18"#]],
    );
    check(
        r#"
L = [1, {"x": len}]
X = json.encode(L)"#,
        &expect![[r#"
            text.star:2:17: `json.encode` can't encode a function: give it `None`, bools, numbers and strings, and lists, tuples and dicts of them
                2 | X = json.encode(L)
                  |                 ^
                the value, a function, is made at text.star:1:15"#]],
    );
    check(
        r#"X = json.encode(image("Dockerfile", context = "//images"))"#,
        &expect![[r#"
            text.star:1:17: `json.encode` can't encode an image: give it `None`, bools, numbers and strings, and lists, tuples and dicts of them
                1 | X = json.encode(image("Dockerfile", context = "//images"))
                  |                 ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        "X = json.encode(json)",
        &expect![[r#"
            text.star:1:17: `json.encode` can't encode the module `json`: give it `None`, bools, numbers and strings, and lists, tuples and dicts of them
                1 | X = json.encode(json)
                  |                 ^^^^"#]],
    );
    check(
        "X = json.encode(x = 1)",
        &expect![[r#"
        text.star:1:17: `json.encode` takes no keyword argument `x`: give it 1 argument, by position
            1 | X = json.encode(x = 1)
              |                 ^"#]],
    );
    check(
        "X = json.encode(1, 2)",
        &expect![[r#"
        text.star:1:5: `json.encode` takes 1 argument, and is given 2: give it 1 argument
            1 | X = json.encode(1, 2)
              |     ^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        "X = json.indent(\"1\")",
        &expect![[r#"
        text.star:1:10: `indent` of the module `json` is not in Pumpkin's Starlark: call one of `decode`, `encode`
            1 | X = json.indent("1")
              |          ^^^^^^
            the receiver, the module `json`, is made at text.star:1:5"#]],
    );
    check(
        "X = json.dumps(1)",
        &expect![[r#"
        text.star:1:10: `dumps` is no method of the module `json`: call one of `decode`, `encode`
            1 | X = json.dumps(1)
              |          ^^^^^
            the receiver, the module `json`, is made at text.star:1:5"#]],
    );
    check(
        "F = json.encode",
        &expect![[r#"
        text.star:1:10: a method named but not called is not in Pumpkin's Starlark: call it here, as `.encode(…)`
            1 | F = json.encode
              |          ^^^^^^"#]],
    );
    check(
        "X = {json: 1}",
        &expect![[r#"
            text.star:1:6: the module `json` can't be a dict's key: key a dict with strings or numbers
                1 | X = {json: 1}
                  |      ^^^^"#]],
    );
    check(
        "X = json.decode(\"1\", None)",
        &expect![[r#"
        text.star:1:5: `json.decode` with 2 arguments is not in Pumpkin's Starlark: give it 1 argument
            1 | X = json.decode("1", None)
              |     ^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        "X = json.decode(\"1\", default = None)",
        &expect![[r#"
        text.star:1:22: `json.decode`'s `default` is not in Pumpkin's Starlark: give `json.decode` 1 argument, by position
            1 | X = json.decode("1", default = None)
              |                      ^^^^^^^"#]],
    );
    check(
        "X = json.decode(1)",
        &expect![[r#"
        text.star:1:17: `json.decode`'s argument can't be `1`: give a string
            1 | X = json.decode(1)
              |                 ^"#]],
    );
    // Each a Starlark literal of a text that `json.decode` refuses.
    let texts = [
        r#""""#,
        r#"" \n""#,
        r#""truefalse""#,
        r#""nul""#,
        r#""'a'""#,
        r#""+1""#,
        r#""NaN""#,
        r#""-Infinity""#,
        r#""1e999""#,
        r#""-""#,
        r#""00""#,
        r#""-01""#,
        r#""1.2.3""#,
        r#""9223372036854775808""#,
        r#""[1, 2""#,
        r#""[1, 2, ]""#,
        r#""[1, 2}""#,
        r#"'{"a" 1}'"#,
        r#"'{1: 2}'"#,
        r#"'{"a": 1, "b": 2, "a": 3}'"#,
        r#"'{"a": 1]'"#,
        r#"'"abc'"#,
        r#"'"a\\gc"'"#,
        r#"'"ab\\u12"'"#,
        r#"'"\\ud83d"'"#,
        r#"'"\\ud83d\\u0041"'"#,
        r#"'"\\ude39"'"#,
        r#"'"a\tb"'"#,
        r#""--1""#,
        r#""-+1""#,
        r#""-abc""#,
        r#""-00""#,
        r#""0123""#,
        r#""-0123""#,
        r#""000.123""#,
        r#""-000.123""#,
        r#""0x123""#,
        r#"".5""#,
        r#"".5e1""#,
        r#""5.""#,
        r#""5.e1""#,
        r#""5e""#,
        r#""5ee1""#,
        r#""1e1e1""#,
        r#""[1, 2 ""#,
        r#""[1, 2, ""#,
        r#""[1, 2, }""#,
        r#"'{"one": 1'"#,
        r#"'{"one": 1 "two": 2'"#,
        r#"'{"one": 1,'"#,
        r#"'{"one": 1, }'"#,
        r#"'"\\u123"'"#,
        r#"'"\\u123'"#,
        r#"'"\\u1'"#,
        r#"'"\\udc8d\\ud83d"'"#,
    ];
    let shown: Vec<String> = (texts.iter())
        .map(|text| shown(Bounds::WORKFLOW, &format!("X = json.decode({text})")))
        .collect();
    expect![[r#"
        text.star:1:17: `json.decode` reads the text's end at offset 0 of its text, where JSON has a value: give it JSON text
            1 | X = json.decode("")
              |                 ^^
        text.star:1:17: `json.decode` reads the text's end at offset 2 of its text, where JSON has a value: give it JSON text
            1 | X = json.decode(" \n")
              |                 ^^^^^
        text.star:1:17: `json.decode` reads `f` at offset 4 of its text, where JSON has the end of the text: give it JSON text
            1 | X = json.decode("truefalse")
              |                 ^^^^^^^^^^^
        text.star:1:17: `json.decode` reads `n` at offset 0 of its text, where JSON has a value: give it JSON text
            1 | X = json.decode("nul")
              |                 ^^^^^
        text.star:1:17: `json.decode` reads `'` at offset 0 of its text, where JSON has a value: give it JSON text
            1 | X = json.decode("'a'")
              |                 ^^^^^
        text.star:1:17: `json.decode` reads `+` at offset 0 of its text, where JSON has a value: give it JSON text
            1 | X = json.decode("+1")
              |                 ^^^^
        text.star:1:17: `json.decode` reads `N` at offset 0 of its text, where JSON has a value: give it JSON text
            1 | X = json.decode("NaN")
              |                 ^^^^^
        text.star:1:17: `json.decode` reads `-` at offset 0 of its text, which is no JSON number a float holds: write a number as JSON does, of at most 1.7976931348623157e+308
            1 | X = json.decode("-Infinity")
              |                 ^^^^^^^^^^^
        text.star:1:17: `json.decode` reads `1e999` at offset 0 of its text, which is no JSON number a float holds: write a number as JSON does, of at most 1.7976931348623157e+308
            1 | X = json.decode("1e999")
              |                 ^^^^^^^
        text.star:1:17: `json.decode` reads `-` at offset 0 of its text, which is no JSON number a float holds: write a number as JSON does, of at most 1.7976931348623157e+308
            1 | X = json.decode("-")
              |                 ^^^
        text.star:1:17: `json.decode` reads `00` at offset 0 of its text, which is no JSON number a float holds: write a number as JSON does, of at most 1.7976931348623157e+308
            1 | X = json.decode("00")
              |                 ^^^^
        text.star:1:17: `json.decode` reads `-01` at offset 0 of its text, which is no JSON number a float holds: write a number as JSON does, of at most 1.7976931348623157e+308
            1 | X = json.decode("-01")
              |                 ^^^^^
        text.star:1:17: `json.decode` reads `1.2.3` at offset 0 of its text, which is no JSON number a float holds: write a number as JSON does, of at most 1.7976931348623157e+308
            1 | X = json.decode("1.2.3")
              |                 ^^^^^^^
        text.star:1:17: `json.decode` of `9223372036854775808`, at offset 0, outside -2^63 to 2^63 - 1, is not in Pumpkin's Starlark: give smaller ints, since ints are 64-bit
            1 | X = json.decode("9223372036854775808")
              |                 ^^^^^^^^^^^^^^^^^^^^^
        text.star:1:17: `json.decode` reads the text's end at offset 5 of its text, where JSON has `,` or `]`: give it JSON text
            1 | X = json.decode("[1, 2")
              |                 ^^^^^^^
        text.star:1:17: `json.decode` reads `]` at offset 7 of its text, where JSON has a value: give it JSON text
            1 | X = json.decode("[1, 2, ]")
              |                 ^^^^^^^^^^
        text.star:1:17: `json.decode` reads `}` at offset 5 of its text, where JSON has `,` or `]`: give it JSON text
            1 | X = json.decode("[1, 2}")
              |                 ^^^^^^^^
        text.star:1:17: `json.decode` reads `1` at offset 5 of its text, where JSON has `:`: give it JSON text
            1 | X = json.decode('{"a" 1}')
              |                 ^^^^^^^^^
        text.star:1:17: `json.decode` reads `1` at offset 1 of its text, where JSON has a key, a string: give it JSON text
            1 | X = json.decode('{1: 2}')
              |                 ^^^^^^^^
        text.star:1:17: `json.decode` of an object that gives its key `"a"` a second time, at offset 17, is not in Pumpkin's Starlark: give each key once
            1 | X = json.decode('{"a": 1, "b": 2, "a": 3}')
              |                 ^^^^^^^^^^^^^^^^^^^^^^^^^^
        text.star:1:17: `json.decode` reads `]` at offset 7 of its text, where JSON has `,` or `}`: give it JSON text
            1 | X = json.decode('{"a": 1]')
              |                 ^^^^^^^^^^
        text.star:1:17: `json.decode` reads the text's end at offset 4 of its text, where JSON has `"`, which closes the string: give it JSON text
            1 | X = json.decode('"abc')
              |                 ^^^^^^
        text.star:1:17: `json.decode` reads `g` at offset 3 of its text, where JSON has an escape: `"`, `\`, `/`, `b`, `f`, `n`, `r`, `t` or `u`: give it JSON text
            1 | X = json.decode('"a\\gc"')
              |                 ^^^^^^^^^
        text.star:1:17: `json.decode` reads `\"` at offset 7 of its text, where JSON has a hexadecimal digit, as `\u` takes four: give it JSON text
            1 | X = json.decode('"ab\\u12"')
              |                 ^^^^^^^^^^^
        text.star:1:17: `json.decode` of a string that holds a UTF-16 surrogate of no pair, escaped at offset 1, is not in Pumpkin's Starlark: escape a character outside the Basic Multilingual Plane as a surrogate pair
            1 | X = json.decode('"\\ud83d"')
              |                 ^^^^^^^^^^^
        text.star:1:17: `json.decode` of a string that holds a UTF-16 surrogate of no pair, escaped at offset 1, is not in Pumpkin's Starlark: escape a character outside the Basic Multilingual Plane as a surrogate pair
            1 | X = json.decode('"\\ud83d\\u0041"')
              |                 ^^^^^^^^^^^^^^^^^^
        text.star:1:17: `json.decode` of a string that holds a UTF-16 surrogate of no pair, escaped at offset 1, is not in Pumpkin's Starlark: escape a character outside the Basic Multilingual Plane as a surrogate pair
            1 | X = json.decode('"\\ude39"')
              |                 ^^^^^^^^^^^
        text.star:1:17: `json.decode` reads `\t` at offset 2 of its text, where JSON has a character of the string, or an escape for it: give it JSON text
            1 | X = json.decode('"a\tb"')
              |                 ^^^^^^^^
        text.star:1:17: `json.decode` reads `--1` at offset 0 of its text, which is no JSON number a float holds: write a number as JSON does, of at most 1.7976931348623157e+308
            1 | X = json.decode("--1")
              |                 ^^^^^
        text.star:1:17: `json.decode` reads `-+1` at offset 0 of its text, which is no JSON number a float holds: write a number as JSON does, of at most 1.7976931348623157e+308
            1 | X = json.decode("-+1")
              |                 ^^^^^
        text.star:1:17: `json.decode` reads `-` at offset 0 of its text, which is no JSON number a float holds: write a number as JSON does, of at most 1.7976931348623157e+308
            1 | X = json.decode("-abc")
              |                 ^^^^^^
        text.star:1:17: `json.decode` reads `-00` at offset 0 of its text, which is no JSON number a float holds: write a number as JSON does, of at most 1.7976931348623157e+308
            1 | X = json.decode("-00")
              |                 ^^^^^
        text.star:1:17: `json.decode` reads `0123` at offset 0 of its text, which is no JSON number a float holds: write a number as JSON does, of at most 1.7976931348623157e+308
            1 | X = json.decode("0123")
              |                 ^^^^^^
        text.star:1:17: `json.decode` reads `-0123` at offset 0 of its text, which is no JSON number a float holds: write a number as JSON does, of at most 1.7976931348623157e+308
            1 | X = json.decode("-0123")
              |                 ^^^^^^^
        text.star:1:17: `json.decode` reads `000.123` at offset 0 of its text, which is no JSON number a float holds: write a number as JSON does, of at most 1.7976931348623157e+308
            1 | X = json.decode("000.123")
              |                 ^^^^^^^^^
        text.star:1:17: `json.decode` reads `-000.123` at offset 0 of its text, which is no JSON number a float holds: write a number as JSON does, of at most 1.7976931348623157e+308
            1 | X = json.decode("-000.123")
              |                 ^^^^^^^^^^
        text.star:1:17: `json.decode` reads `x` at offset 1 of its text, where JSON has the end of the text: give it JSON text
            1 | X = json.decode("0x123")
              |                 ^^^^^^^
        text.star:1:17: `json.decode` reads `.` at offset 0 of its text, where JSON has a value: give it JSON text
            1 | X = json.decode(".5")
              |                 ^^^^
        text.star:1:17: `json.decode` reads `.` at offset 0 of its text, where JSON has a value: give it JSON text
            1 | X = json.decode(".5e1")
              |                 ^^^^^^
        text.star:1:17: `json.decode` reads `5.` at offset 0 of its text, which is no JSON number a float holds: write a number as JSON does, of at most 1.7976931348623157e+308
            1 | X = json.decode("5.")
              |                 ^^^^
        text.star:1:17: `json.decode` reads `5.e1` at offset 0 of its text, which is no JSON number a float holds: write a number as JSON does, of at most 1.7976931348623157e+308
            1 | X = json.decode("5.e1")
              |                 ^^^^^^
        text.star:1:17: `json.decode` reads `5e` at offset 0 of its text, which is no JSON number a float holds: write a number as JSON does, of at most 1.7976931348623157e+308
            1 | X = json.decode("5e")
              |                 ^^^^
        text.star:1:17: `json.decode` reads `5ee1` at offset 0 of its text, which is no JSON number a float holds: write a number as JSON does, of at most 1.7976931348623157e+308
            1 | X = json.decode("5ee1")
              |                 ^^^^^^
        text.star:1:17: `json.decode` reads `1e1e1` at offset 0 of its text, which is no JSON number a float holds: write a number as JSON does, of at most 1.7976931348623157e+308
            1 | X = json.decode("1e1e1")
              |                 ^^^^^^^
        text.star:1:17: `json.decode` reads the text's end at offset 6 of its text, where JSON has `,` or `]`: give it JSON text
            1 | X = json.decode("[1, 2 ")
              |                 ^^^^^^^^
        text.star:1:17: `json.decode` reads the text's end at offset 7 of its text, where JSON has a value: give it JSON text
            1 | X = json.decode("[1, 2, ")
              |                 ^^^^^^^^^
        text.star:1:17: `json.decode` reads `}` at offset 7 of its text, where JSON has a value: give it JSON text
            1 | X = json.decode("[1, 2, }")
              |                 ^^^^^^^^^^
        text.star:1:17: `json.decode` reads the text's end at offset 9 of its text, where JSON has `,` or `}`: give it JSON text
            1 | X = json.decode('{"one": 1')
              |                 ^^^^^^^^^^^
        text.star:1:17: `json.decode` reads `\"` at offset 10 of its text, where JSON has `,` or `}`: give it JSON text
            1 | X = json.decode('{"one": 1 "two": 2')
              |                 ^^^^^^^^^^^^^^^^^^^^
        text.star:1:17: `json.decode` reads the text's end at offset 10 of its text, where JSON has a key, a string: give it JSON text
            1 | X = json.decode('{"one": 1,')
              |                 ^^^^^^^^^^^^
        text.star:1:17: `json.decode` reads `}` at offset 11 of its text, where JSON has a key, a string: give it JSON text
            1 | X = json.decode('{"one": 1, }')
              |                 ^^^^^^^^^^^^^^
        text.star:1:17: `json.decode` reads `\"` at offset 6 of its text, where JSON has a hexadecimal digit, as `\u` takes four: give it JSON text
            1 | X = json.decode('"\\u123"')
              |                 ^^^^^^^^^^
        text.star:1:17: `json.decode` reads the text's end at offset 6 of its text, where JSON has a hexadecimal digit, as `\u` takes four: give it JSON text
            1 | X = json.decode('"\\u123')
              |                 ^^^^^^^^^
        text.star:1:17: `json.decode` reads the text's end at offset 4 of its text, where JSON has a hexadecimal digit, as `\u` takes four: give it JSON text
            1 | X = json.decode('"\\u1')
              |                 ^^^^^^^
        text.star:1:17: `json.decode` of a string that holds a UTF-16 surrogate of no pair, escaped at offset 1, is not in Pumpkin's Starlark: escape a character outside the Basic Multilingual Plane as a surrogate pair
            1 | X = json.decode('"\\udc8d\\ud83d"')
              |                 ^^^^^^^^^^^^^^^^^^"#]].assert_eq(&shown.join("\n"));
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshots"
)]
fn labels() {
    // A task's file loads the standard library's, which loads its own; an image's context and
    // mounts may be its directories.
    check(
        r#"
#- //tasks/bench/task.star
load("//lib/parts.star", "AGENT", "FINAL_TARGET", "FINAL")
load("@pumpkin//harness/agent.star", "agent")

workflow(name = "neutral/library", roles = [AGENT | {"image": agent()}, FINAL_TARGET], final = FINAL, deadline_seconds = 3600)
#- @pumpkin//harness/agent.star
load("@pumpkin//harness/names.star", "NAME")

def agent():
    mount = {"m": "@pumpkin//harness/two"}
    return image("Dockerfile", context = "@pumpkin//harness/one", mount = mount, args = {"N": NAME})
#- @pumpkin//harness/names.star
NAME = "one"
"#,
        &expect![[r#"
            image 1: @pumpkin//harness/one's Dockerfile, with @pumpkin//harness/two at m, with N="one", in pumpkin-library/harness/one
            building 1
            {
              "deadline_seconds": 3600,
              "final": {
                "body": {
                  "def": "final_call",
                  "file": "//lib/parts.star"
                },
                "payload_max_bytes": 65536,
                "target": "final_judge",
                "timeout_seconds": 120
              },
              "ir_version": "pumpkin.ir/v1",
              "name": "neutral/library",
              "roles": [
                {
                  "image": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb1",
                  "name": "agent",
                  "needs": {
                    "cpu_millis": 2000,
                    "memory_bytes": 4294967296
                  },
                  "runtime": {
                    "agent": {
                      "command": {
                        "argv": [
                          "/usr/local/bin/agent"
                        ],
                        "cwd": "/workspace"
                      },
                      "finalize_grace_seconds": 0,
                      "grants": [
                        {
                          "name": "final",
                          "now": {
                            "body": {
                              "arguments": {
                                "finalize": {
                                  "default": false,
                                  "type": "bool"
                                }
                              },
                              "def": "submit",
                              "file": "//lib/submit.star"
                            },
                            "payload_max_bytes": 65536
                          }
                        }
                      ],
                      "stdin": "Solve the task in /task/README.md.\n"
                    }
                  },
                  "user": {
                    "gid": 1000,
                    "uid": 1000
                  }
                },
                {
                  "image": "sha256:2222222222222222222222222222222222222222222222222222222222222222",
                  "name": "final_judge",
                  "needs": {
                    "cpu_millis": 1000,
                    "memory_bytes": 536870912
                  },
                  "runtime": {
                    "target": {
                      "cwd": "/pumpkin/home"
                    }
                  },
                  "user": {
                    "gid": 65532,
                    "uid": 65532
                  }
                }
              ]
            }"#]],
    );
    check(
        r#"
#- //tasks/bench/task.star
load("@pumpkin//harness/agent.star", "AGENT")
#- @pumpkin//harness/agent.star
load("//lib/parts.star", "AGENT")
load("@pumpkin//harness/names.star", "NAME")
"#,
        &expect![[r#"
            @pumpkin//harness/agent.star:1:6: a load of a task's file, `//lib/parts.star`, in a file of the standard library, which depends on no task, is not in Pumpkin's Starlark: load only `@pumpkin//` files there
                1 | load("//lib/parts.star", "AGENT")
                  |      ^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"
#- //tasks/bench/task.star
load("@pumpkin//harness/agent.star", "AGENT")
#- @pumpkin//harness/agent.star
AGENT = 1 + "a"
"#,
        &expect![[r#"
            @pumpkin//harness/agent.star:1:11: `+` can't take an int and a string: give it two numbers, two strings, two lists or two tuples
                1 | AGENT = 1 + "a"
                  |           ^
                the left operand, `1`, is made at @pumpkin//harness/agent.star:1:9
                the right operand, `"a"`, is made at @pumpkin//harness/agent.star:1:13"#]],
    );
    // `.` in a library's file would name a directory below the task's root.
    check(
        r#"
#- @pumpkin//harness/entry.star
X = image("Dockerfile", context = ".")
"#,
        &expect![
            "@pumpkin//harness/entry.star: a file of the standard library can't be an entry, whose directory is below the task's root: evaluate a `//` file"
        ],
    );
    check(
        r#"X = image("Dockerfile", context = "@x")"#,
        &expect![[r#"
            text.star:1:35: `@x` names no directory below the root: write `.` for the entry file's directory, a path below it, or `//` and a path below the root, each in `/`-separated components of ASCII letters, digits, `.`, `_` and `-`, none of them empty, `.` or `..`
                1 | X = image("Dockerfile", context = "@x")
                  |                                   ^^^^"#]],
    );
    check(
        r#"load("@pumpkin//harness/none.star", "X")"#,
        &expect![[r#"
        text.star:1:6: loading `@pumpkin//harness/none.star`: no fixture is at `@pumpkin//harness/none.star`: name a file in the fixtures
            1 | load("@pumpkin//harness/none.star", "X")
              |      ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"load("@other//harness/agent.star", "X")"#,
        &expect![[r#"
        text.star:1:6: `@other//harness/agent.star` names a root Pumpkin has none of: start it with `//`, the task's root, or `@pumpkin//`, the standard library's
            1 | load("@other//harness/agent.star", "X")
              |      ^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"load("lib//x.star", "X")"#,
        &expect![[r#"
        text.star:1:6: `lib//x.star` is no label: start it with `//`, the root
            1 | load("lib//x.star", "X")
              |      ^^^^^^^^^^^^^"#]],
    );
    check(
        r#"load("@pumpkin//../agent.star", "X")"#,
        &expect![[r#"
        text.star:1:6: `@pumpkin//../agent.star` is no label: after `@pumpkin//`, write `/`-separated components of ASCII letters, digits, `.`, `_` and `-`, none of them empty, `.` or `..`
            1 | load("@pumpkin//../agent.star", "X")
              |      ^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = image("Dockerfile", context = "@pumpkin//harness/../one")"#,
        &expect![[r#"
            text.star:1:35: `@pumpkin//harness/../one` is no label: after `@pumpkin//`, write `/`-separated components of ASCII letters, digits, `.`, `_` and `-`, none of them empty, `.` or `..`
                1 | X = image("Dockerfile", context = "@pumpkin//harness/../one")
                  |                                   ^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = image("Dockerfile", context = "@other//harness")"#,
        &expect![[r#"
            text.star:1:35: `@other//harness` names a root Pumpkin has none of: start it with `//`, the task's root, or `@pumpkin//`, the standard library's
                1 | X = image("Dockerfile", context = "@other//harness")
                  |                                   ^^^^^^^^^^^^^^^^^"#]],
    );
}
