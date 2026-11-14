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

mod conformance;
mod fixtures;

use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroUsize;
use std::path::Path;
use std::{fs, thread};

use expect_test::{Expect, expect};
use lang::{Args, Bounds, Declared, Label, Loader, Problems, Recipe};
use schema::ir;
use schema::workflow::Workflow;

/// Each file of `tests/fixtures`, by its path below it.
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
    /// `--arg`, then a line `#- //<label>` above each file, which holds the lines below it up to
    /// the next; with no such line, the rest is the file `//text.star`. The first file is the
    /// entry; a label with no line below it names one of `files` as the entry, the case holding
    /// none of its text.
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
                let path = (label.strip_prefix("#- //")).unwrap_or_else(|| {
                    panic!("`{label}` is no line of a fixture: write `#- //<label>` above a file")
                });
                Label::parse(&format!("//{path}")).unwrap();
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

    /// How many prefixes of the file at `path` evaluate, given `args`, each cut at a
    /// character's boundary, with the other files whole; one whose problems don't read panics,
    /// named beside the panic.
    fn prefixes(&self, path: &str, args: &Args) -> usize {
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
        let ends = text.char_indices().map(|(end, _)| end).chain([text.len()]);
        ends.filter(|&end| {
            let _named = Named { path, end };
            *cut.held.last_mut().unwrap() = (path, &text[..end]);
            cut.evaluate(args).is_ok()
        })
        .count()
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

/// The files a case holds, then those of `tests/fixtures`, as a loader reads them.
struct Sources<'s> {
    held: &'s [(&'s str, &'s str)],
    files: &'s BTreeMap<String, String>,
}

impl Loader for Sources<'_> {
    fn load(&mut self, label: &Label) -> Result<String, String> {
        let path = label.path();
        if let Some((_, text)) = self.held.iter().find(|(held, _)| *held == path) {
            return Ok((*text).to_owned());
        }
        (self.files.get(path).cloned())
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

/// Checks the case that `fixture` writes, as [`check_within`] does, within a workflow's bounds.
#[track_caller]
fn check(fixture: &str, expect: &Expect) {
    check_within(Bounds::WORKFLOW, fixture, expect);
}

/// Checks what the case that `fixture` writes, as `Case::parse` reads it, within `bounds`, shows
/// against
/// `expect`, with each carriage return written as `\r`, and each of U+2028 to U+202E as
/// `\u{…}`: what it evaluates to, or the problems
/// of its `--arg`s; and that every prefix of each file it holds evaluates or gives problems
/// that read.
#[track_caller]
fn check_within(bounds: Bounds, fixture: &str, expect: &Expect) {
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
            case.prefixes(path, &args);
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
    expect.assert_eq(&shown);
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
/// image's build, which
/// a line of `built` describes, as does each step of the builds, by the images it builds at
/// once: a stand-in check refuses each recipe of the package `bad/refused`, naming its named
/// contexts, and each of its mounts, and a stand-in build, of [`JOBS`] images at once, the
/// oldest ending at each step, fails each of `bad/broken`'s.
fn run(
    path: &str,
    loader: &mut impl Loader,
    args: &Args,
    bounds: Bounds,
    built: &mut Vec<String>,
) -> Result<Workflow, Problems> {
    let entry = Label::parse(&format!("//{path}")).unwrap();
    let plan = lang::evaluate(&entry, loader, args, bounds)?;
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
    let line = match declared {
        Declared::Recipe {
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
        } => {
            if context.path() == "bad/broken" {
                return Err(format!("the stand-in build fails `{recipe}`"));
            }
            let mounts = (mounts.iter()).map(|(at, dir)| format!(", with {dir} at {at}"));
            let target = target.iter().map(|target| format!(", its stage {target}"));
            let args = (args.iter()).map(|(name, value)| format!(", with {name}={value:?}"));
            let contexts = (contexts.iter())
                .map(|(name, built)| format!(", with image {} as {name}", built.number));
            let given: String = mounts.chain(target).chain(args).chain(contexts).collect();
            format!("{context}'s {recipe}{given}, in {repository}")
        }
        Declared::Layer {
            recipe: _,
            contexts,
            repository,
        } => {
            let contexts: Vec<String> = (contexts.iter())
                .map(|(name, built)| format!(", with image {} as {name}", built.number))
                .collect();
            format!("a harness's layer{}, in {repository}", contexts.concat())
        }
    };
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
ROLES = [AGENT, JUDGE]

load("//lib/parts.star", "SCORE")

workflow(name = "neutral/names", roles = ROLES, score = SCROE)
"#,
        &expect![[r#"
            bad/names/task.star:1:35: `_DRAFT` is private to `//lib/parts.star`: load a name that doesn't start with `_`
                1 | load("//lib/parts.star", "AGENT", "_DRAFT")
                  |                                   ^^^^^^^^
            bad/names/task.star:4:1: `ROLES` is bound already: bind each global once
                4 | ROLES = [AGENT, JUDGE]
                  | ^^^^^
                it is first bound at bad/names/task.star:3:1
            bad/names/task.star:4:17: `JUDGE` is not defined: bind it in this file, or load it
                4 | ROLES = [AGENT, JUDGE]
                  |                 ^^^^^
            bad/names/task.star:6:1: a `load` after other statements is not in Pumpkin's Starlark: put every `load` at the top of the file
                6 | load("//lib/parts.star", "SCORE")
                  | ^^^^
                the first other statement is at bad/names/task.star:3:1
            bad/names/task.star:8:57: `SCROE` is not defined: bind it in this file, or load it
                8 | workflow(name = "neutral/names", roles = ROLES, score = SCROE)
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
load("//lib/parts.star", "AGENT", "JUDGE", "SCORE")

workflow(
    name = "neutral/status",
    roles = [AGENT, JUDGE | {"status": "ready"}],
    score = SCORE,
)
"#,
        &expect![[r#"
            bad/status/task.star:5:30: `status` is no field of an element of `roles`: expected one of `name`, `image`, `user`, `needs`, `runtime`, `egress`, `grants`
                5 |     roles = [AGENT, JUDGE | {"status": "ready"}],
                  |                              ^^^^^^^^
                the IR's /roles/1"#]],
    );
    check(
        r#"
#- //bad/zero/task.star
load("//lib/parts.star", "AGENT", "JUDGE", "SCORE")

CPU_MILLIS = 0

workflow(
    name = "neutral/zero",
    roles = [AGENT, JUDGE | {"needs": {"cpu_millis": CPU_MILLIS, "memory_bytes": 512 * MiB}}],
    score = SCORE,
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
#- //bad/slot/task.star
load("//lib/parts.star", "AGENT", "JUDGE", "SCORE")

# Over the op payload limit, 64 MiB, which holds every slot.
SLOT = {"name": "payload", "max_bytes": 65 * MiB}

# Each judge that holds it is refused, at the place it is made.
RUNTIME = {"judge": JUDGE["runtime"]["judge"] | {"slots": [SLOT]}}

workflow(
    name = "neutral/slot",
    roles = [
        AGENT,
        JUDGE | {"name": "train_judge", "runtime": RUNTIME},
        JUDGE | {"runtime": RUNTIME},
    ],
    score = SCORE,
)
"#,
        &expect![[r#"
            bad/slot/task.star:4:41: slot `payload`'s `max_bytes` is 68157440, over the op payload limit of 67108864: give at most that
                4 | SLOT = {"name": "payload", "max_bytes": 65 * MiB}
                  |                                         ^^^^^^^^
                the IR's /roles/1/runtime/judge/slots/0/max_bytes
                the IR's /roles/2/runtime/judge/slots/0/max_bytes"#]],
    );
    check(
        r#"
#- //bad/keyword/task.star
load("//lib/judged.star", "judged_task")

judged_task(
    task = "tasks/keyword",
    agent = {"cpu_millis": 4000, "memory_bytes": 8 * GiB},
    metric = ["solved"],
)
"#,
        &expect![[r#"
            bad/keyword/task.star:6:5: `metric` is no parameter of `judged_task`: expected one of `task`, `agent`, `metrics`, `calls`, `judge`, `env`, `models`
                6 |     metric = ["solved"],
                  |     ^^^^^^"#]],
    );
    check(
        r#"
#- //bad/operand/task.star
load("//lib/judged.star", "judged_task")

judged_task(
    task = "tasks/operand",
    agent = {"cpu_millis": 4000, "memory_bytes": 8 * GiB},
    metrics = ["solved"],
    env = {"SEED": 7},
)
"#,
        &expect![[r#"
            lib/judged.star:27:35: `+` can't take a string and an int: give it two ints, two strings, two lists or two tuples
                27 |                 "env": [key + "=" + value for key, value in env.items()] + [
                   |                                   ^
                the left operand, `"SEED="`, is made at lib/judged.star:27:25
                the right operand, `7`, is made at bad/operand/task.star:7:20
                in `_judge()`, called at lib/judged.star:88:13
                in `judged_task()`, called at bad/operand/task.star:3:1"#]],
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
            text.star:1:10: `<` of two tuples is not in Pumpkin's Starlark: order two ints or two strings
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
            text.star:1:5: unary `+` is not in Pumpkin's Starlark: write the int alone
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
        "workflow(**X)",
        &expect![[r#"
            text.star:1:10: `**` before an argument, which passes a dict's entries, is not in Pumpkin's Starlark: name each keyword argument
                1 | workflow(**X)
                  |          ^^"#]],
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
            text.star:1:2: type annotations are not in Starlark: drop the `:` and the type
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
        "X = 1.5",
        &expect![[r#"
            text.star:1:5: `1.5`, a float, is not in Pumpkin's Starlark: write an int
                1 | X = 1.5
                  |     ^^^"#]],
    );
    check(
        "X = .5",
        &expect![[r#"
            text.star:1:5: `.5`, a float, is not in Pumpkin's Starlark: write an int
                1 | X = .5
                  |     ^^"#]],
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
        r#"X = "\u00e9""#,
        &expect![[r#"
        text.star:1:6: `\u`, an escape that some Starlark implementations lack, is not in Pumpkin's Starlark: write the character itself, or an ASCII one in octal, as `\101` for `A`
            1 | X = "\u00e9"
              |      ^^"#]],
    );
    check(
        r#"X = '\U0001F600'"#,
        &expect![[r#"
        text.star:1:6: `\U`, an escape that some Starlark implementations lack, is not in Pumpkin's Starlark: write the character itself, or an ASCII one in octal, as `\101` for `A`
            1 | X = '\U0001F600'
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
        "X = 1 / 2",
        &expect![[r#"
            text.star:1:7: `/`, which divides into a float, is not in Pumpkin's Starlark: divide with `//`
                1 | X = 1 / 2
                  |       ^"#]],
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
            text.star:1:5: `type`, a builtin of Starlark's, is not in Pumpkin's Starlark: compute the value with `bool`, `int`, `len`, `sorted` and `str`
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
            text.star:1:6: a list can't be a dict's key: key a dict with strings or ints
                1 | X = {[1]: 2}
                  |      ^^^"#]],
    );
    check(
        "X = {None: 1}",
        &expect![[r#"
        text.star:1:6: `None` as a dict's key is not in Pumpkin's Starlark: key a dict with strings or ints
            1 | X = {None: 1}
              |      ^^^^"#]],
    );
    check(
        "X = {(1, 2): 1}",
        &expect![[r#"
        text.star:1:6: a tuple as a dict's key is not in Pumpkin's Starlark: key a dict with strings or ints
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
            text.star:1:6: a list can't be a dict's key: key a dict with strings or ints
                1 | X = {[x]: x for x in [1]}
                  |      ^^^"#]],
    );
    check(
        r#"X = "N=" + 3"#,
        &expect![[r#"
            text.star:1:10: `+` can't take a string and an int: give it two ints, two strings, two lists or two tuples
                1 | X = "N=" + 3
                  |          ^
                the left operand, `"N="`, is made at text.star:1:5
                the right operand, `3`, is made at text.star:1:12"#]],
    );
    check(
        r#"X = -"a""#,
        &expect![[r#"
            text.star:1:5: `-` can't take a string: give it an int
                1 | X = -"a"
                  |     ^
                the operand, `"a"`, is made at text.star:1:6"#]],
    );
    check(
        r#"X = 1 < "a""#,
        &expect![[r#"
            text.star:1:7: `<` can't take an int and a string: give it two ints or two strings
                1 | X = 1 < "a"
                  |       ^
                the left operand, `1`, is made at text.star:1:5
                the right operand, `"a"`, is made at text.star:1:9"#]],
    );
    check(
        "X = True < False",
        &expect![[r#"
        text.star:1:10: `<` of two bools is not in Pumpkin's Starlark: order two ints or two strings
            1 | X = True < False
              |          ^
            the left operand, `True`, is made at text.star:1:5
            the right operand, `False`, is made at text.star:1:12"#]],
    );
    check(
        "X = [1] < [2]",
        &expect![[r#"
            text.star:1:9: `<` of two lists is not in Pumpkin's Starlark: order two ints or two strings
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
            text.star:1:9: a list can't be a dict's key: key a dict with strings or ints
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
        r#"X = {"a": 1}[[1]]"#,
        &expect![[r#"
            text.star:1:14: a list can't be a dict's key: key a dict with strings or ints
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
        "X = len([], [])",
        &expect![[r#"
            text.star:1:5: `len` takes 1 argument, and is given 2: give it 1 argument
                1 | X = len([], [])
                  |     ^^^^^^^^^^^"#]],
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
            text.star:1:9: `str` of a list is not in Pumpkin's Starlark: give it an int or a string
                1 | X = str([1])
                  |         ^^^"#]],
    );
    check(
        r#"X = int("1_000")"#,
        &expect![[r#"
            text.star:1:9: `int` can't read `"1_000"`: give it decimal digits, after `-` or `+` or neither
                1 | X = int("1_000")
                  |         ^^^^^^^"#]],
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
            text.star:1:9: `int` can't take a list: give it an int, a bool or a string of digits
                1 | X = int([])
                  |         ^^"#]],
    );
    check(
        r#"X = sorted([1, "a"])"#,
        &expect![[r#"
            text.star:1:12: `sorted` of a list of mixed or other values than ints or strings is not in Pumpkin's Starlark: give it only ints, or only strings
                1 | X = sorted([1, "a"])
                  |            ^^^^^^^^"#]],
    );
    check(
        "X = sorted([True])",
        &expect![[r#"
            text.star:1:12: `sorted` of a list of mixed or other values than ints or strings is not in Pumpkin's Starlark: give it only ints, or only strings
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
            text.star:1:6: `fail` of a list is not in Pumpkin's Starlark: give it an int or a string
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
load("//lib/parts.star", "SCORE")
X = SCORE.update({})"#,
        &expect![[r#"
            text.star:2:11: `update`, which would change the dict, is not in Pumpkin's Starlark: make a new dict with `|`, as values never change
                2 | X = SCORE.update({})
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
        r#"X = "a".replace("a", "b", 1, 2)"#,
        &expect![[r#"
        text.star:1:5: `replace` takes 2 arguments, and is given 4: give it 2 arguments
            1 | X = "a".replace("a", "b", 1, 2)
              |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
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
            text.star:1:6: a tuple that holds an unhashable value can't be a dict's key: key a dict with strings or ints
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
        r#"X = "a".replace(old = "a", new = "b")"#,
        &expect![[r#"
        text.star:1:17: `replace` takes no keyword argument `old`: give it 2 arguments, by position
            1 | X = "a".replace(old = "a", new = "b")
              |                 ^^^"#]],
    );
    check(
        r#"X = "a".startswith(sub = "a")"#,
        &expect![[r#"
        text.star:1:20: `startswith` takes no keyword argument `sub`: give it 1 argument, by position
            1 | X = "a".startswith(sub = "a")
              |                    ^^^"#]],
    );
    check(
        "X = sorted([2, 1], cmp = True)",
        &expect![[r#"
            text.star:1:20: `sorted` takes no keyword argument `cmp`: give it 1 argument, by position
                1 | X = sorted([2, 1], cmp = True)
                  |                    ^^^"#]],
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
            text.star:1:12: a list can't be a dict's key: key a dict with strings or ints
                1 | X = {}.get([1])
                  |            ^^^"#]],
    );
    check(
        "X = {}.get(True)",
        &expect![[r#"
        text.star:1:12: a bool as a dict's key is not in Pumpkin's Starlark: key a dict with strings or ints
            1 | X = {}.get(True)
              |            ^^^^"#]],
    );
    check(
        "X = None in {}",
        &expect![[r#"
        text.star:1:10: `None` as a dict's key is not in Pumpkin's Starlark: key a dict with strings or ints
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
        r#"X = {}.get("a", d = 1)"#,
        &expect![[r#"
            text.star:1:5: `get` takes 1 or 2 arguments, by position: give it a key, and a default or none
                1 | X = {}.get("a", d = 1)
                  |     ^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"X = {}.get("a", 1, d = 2)"#,
        &expect![[r#"
            text.star:1:5: `get` takes 1 or 2 arguments, by position: give it a key, and a default or none
                1 | X = {}.get("a", 1, d = 2)
                  |     ^^^^^^^^^^^^^^^^^^^^^"#]],
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
            text.star:1:17: `format` of a list is not in Pumpkin's Starlark: give it an int or a string
                1 | X = "{}".format([1])
                  |                 ^^^"#]],
    );
    check(
        r#"X = "0123456789012345678901234567890123456789012345678901234567890123" + 1"#,
        &expect![[r#"
            text.star:1:72: `+` can't take a string and an int: give it two ints, two strings, two lists or two tuples
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
            text.star:1:1: `workflow` lacks `roles`, `score`: give them
                1 | workflow(name = "")
                  | ^^^^^^^^^^^^^^^^^^^"#]],
    );
    // Values the IR's types refuse, in the language's words.
    check(
        r#"workflow(name = "n/x", roles = [1], score = None)"#,
        &expect![[r#"
            text.star:1:33: an element of `roles` can't be an int: expected a dict
                1 | workflow(name = "n/x", roles = [1], score = None)
                  |                                 ^
                the IR's /roles/0"#]],
    );
    check(
        r#"workflow(roles = [{"user": {"uid": -1}}], name = "", score = None)"#,
        &expect![[r#"
            text.star:1:36: `uid` can't be `-1`: expected an int from 1 to 4294967295
                1 | workflow(roles = [{"user": {"uid": -1}}], name = "", score = None)
                  |                                    ^^
                the IR's /roles/0/user/uid"#]],
    );
    check(
        r#"workflow(roles = [{"runtime": "agent"}], name = "", score = None)"#,
        &expect![[r#"
            text.star:1:31: `runtime` can't be `"agent"`: expected a dict that gives the kind its value, as `{"agent": …}`
                1 | workflow(roles = [{"runtime": "agent"}], name = "", score = None)
                  |                               ^^^^^^^
                the IR's /roles/0/runtime"#]],
    );
    check(
        r#"workflow(roles = [{"runtime": {"agent": {"finalize_grace_seconds": -1}}}], name = "", score = None)"#,
        &expect![[r#"
            text.star:1:68: `finalize_grace_seconds` can't be `-1`: expected an int of 0 or more
                1 | workflow(roles = [{"runtime": {"agent": {"finalize_grace_seconds": -1}}}], name = "", score = None)
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
fail(",".join([k for k in D] + [k + str(v) for k, v in {k: D[k] for k in D}.items()]))"#,
        &expect![[r#"
            text.star:2:1: fail: b,a,c,d,b1,a4,c3,d5
                2 | fail(",".join([k for k in D] + [k + str(v) for k, v in {k: D[k] for k in D}.items()]))
                  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
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
    workflow(name = "n/x", roles = [], score = None)
X = make()"#,
        &expect![[r#"
            lib.star:2:5: `workflow()` is called while `//lib.star` loads: call it in the entry file, which loads `//lib.star`
                2 |     workflow(name = "n/x", roles = [], score = None)
                  |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
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
def f(x: int):
    pass"#,
        &expect![[r#"
            lib.star:1:8: type annotations are not in Starlark: drop the `:` and the type
                1 | def f(x: int):
                  |        ^"#]],
    );
    check(
        r#"
#- //text.star
load("//lib.star", "X")
#- //lib.star
def f() -> int:
    return 1"#,
        &expect![[r#"
            lib.star:1:9: type annotations are not in Starlark: drop the `->` and the type
                1 | def f() -> int:
                  |         ^"#]],
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
    load("//lib/parts.star", "SCORE")"#,
        &expect![[r#"
            lib.star:2:5: a `load` stands only at a file's top level, above its other statements: move it there
                2 |     load("//lib/parts.star", "SCORE")
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
            image 6: a harness's layer, with image 4 as base, in pumpkin/tasks/images/agent/codex-0.167.0
            building 5 and 6
            building 6
            {
              "ir_version": "pumpkin.ir/v0",
              "name": "neutral/images",
              "roles": [
                {
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
                  "grants": [
                    {
                      "judge": {
                        "call_timeout_seconds": 600,
                        "calls": 10,
                        "name": "train",
                        "role": "train_judge",
                        "slot": "payload"
                      }
                    },
                    {
                      "submit": {
                        "name": "final"
                      }
                    }
                  ],
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
                          "/harness/codex-0.167.0/bin/codex",
                          "exec",
                          "--ephemeral",
                          "--ignore-rules",
                          "--skip-git-repo-check",
                          "--json",
                          "-"
                        ],
                        "cwd": "/workspace",
                        "env": [
                          "CODEX_HOME=/workspace/.codex",
                          "TMPDIR=/workspace/.tmp"
                        ]
                      },
                      "deadline_seconds": 7200,
                      "dirs": [
                        {
                          "mode": "0700",
                          "path": "/workspace/.codex"
                        },
                        {
                          "mode": "0700",
                          "path": "/workspace/.tmp"
                        }
                      ],
                      "files": [
                        {
                          "content": "model = \"model-large\"\nmodel_reasoning_effort = \"high\"\nmodel_provider = \"pumpkin\"\n# Nobody is there to approve a command, and the agent's sandbox confines Codex, whose own\n# sandbox needs `bwrap`, which its pin leaves out.\napproval_policy = \"never\"\nsandbox_mode = \"danger-full-access\"\n# A tool's shell reads no login profile, which could change the environment the command sets.\nallow_login_shell = false\n# Nothing reaches past the model route, the only way out of the sandbox.\nweb_search = \"disabled\"\n\n# Codex's record of the run is the events it writes to its standard output, which the teardown\n# collects. The databases it makes in its home as it starts stay there, uncollected: `exec`\n# writes them no log, and its session, ephemeral, keeps nothing in them.\n\n# The model endpoint `sandboxd` serves, which needs no credential.\n[model_providers.pumpkin]\n# Any name but `OpenAI` keeps off the features Codex keeps for the provider of that name.\nname = \"Pumpkin\"\nbase_url = \"http://127.0.0.1:11235\"\nwire_api = \"responses\"\nrequires_openai_auth = false\n# A run of hours meets transient failures.\nrequest_max_retries = 100\nstream_max_retries = 100\n\n# The lead agent and as many spawned ones at most, each with one model request in flight at a\n# time: as many requests as the model route takes from a sandbox at once.\n[agents]\nmax_concurrent_threads_per_session = 6\n\n[analytics]\nenabled = false\n\n[features]\nplugins = false\napps = false\nimage_generation = false\n\n[plugins._default]\nenabled = false\n",
                          "mode": "0600",
                          "path": "/workspace/.codex/config.toml"
                        }
                      ],
                      "finalize_grace_seconds": 0,
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
                  "name": "train_judge",
                  "needs": {
                    "cpu_millis": 1000,
                    "memory_bytes": 536870912
                  },
                  "runtime": {
                    "judge": {
                      "command": {
                        "argv": [
                          "/opt/judge/run"
                        ],
                        "cwd": "/pumpkin/home"
                      },
                      "result": {
                        "returns": [
                          "/score"
                        ],
                        "score": "/score"
                      },
                      "slots": [
                        {
                          "max_bytes": 65536,
                          "name": "payload"
                        }
                      ]
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
                    "judge": {
                      "command": {
                        "argv": [
                          "/opt/judge/run"
                        ],
                        "cwd": "/pumpkin/home"
                      },
                      "result": {
                        "returns": [
                          "/score"
                        ],
                        "score": "/score"
                      },
                      "slots": [
                        {
                          "max_bytes": 65536,
                          "name": "payload"
                        }
                      ]
                    }
                  },
                  "user": {
                    "gid": 65532,
                    "uid": 65532
                  }
                }
              ],
              "score": {
                "role": "final_judge",
                "slot": "payload",
                "timeout_seconds": 120
              }
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
            lib/images.star:9:12: `--arg deadline` is `"1h"`, which `int` can't read: give it decimal digits, after `-` or `+` or neither, from -2^63 to 2^63 - 1
                9 | DEADLINE = arg("deadline", int, default = 5 * 3600)
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
            lib/images.star:10:10: `--arg strict` is `"yes"`, which is no bool: give it `true` or `false`
                10 | STRICT = arg("strict", bool, default = False)
                   |          ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        &format!(
            "
#- --arg effort=hard
{IMAGES}"
        ),
        &expect![[r#"
            lib/images.star:7:10: `hard` is not a reasoning effort: use one of none, minimal, low, medium, high, xhigh, max, ultra
                7 | EFFORT = arg("effort", str, default = "high")
                  |          ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `images_task()`, called at tasks/images/task.star:5:1"#]],
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
            tasks/images/task.star: `--arg efort` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `strict`, `tools`
            tasks/images/task.star: `--arg mdoel` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `strict`, `tools`"#]],
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
            lib/images.star:38:13: the stand-in check refuses `tools/Dockerfile`
                38 |     tools = image("tools/Dockerfile", args = BASE_ARGS)
                   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `images_task()`, called at bad/refused/task.star:5:1
            lib/images.star:39:17: the stand-in check refuses `toolchain/Dockerfile`
                39 |     toolchain = image("toolchain/Dockerfile", target = "Go")
                   |                 ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `images_task()`, called at bad/refused/task.star:5:1
            lib/images.star:41:19: the stand-in check refuses `agent/Dockerfile`, given build-tools, given toolchain
                41 |     agent_image = image("agent/Dockerfile", context = ".", mount = SHARED, contexts = built_on)
                   |                   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `images_task()`, called at bad/refused/task.star:5:1
            lib/images.star:41:19: the stand-in check refuses the mount at `shared`
                41 |     agent_image = image("agent/Dockerfile", context = ".", mount = SHARED, contexts = built_on)
                   |                   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `images_task()`, called at bad/refused/task.star:5:1
            lib/images.star:24:18: the stand-in check refuses `judge/Dockerfile`
                24 |         "image": image("judge/Dockerfile", mount = SHARED, target = stage, args = BASE_ARGS),
                   |                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `_judge()`, called at lib/images.star:44:15
                in `images_task()`, called at bad/refused/task.star:5:1
            lib/images.star:24:18: the stand-in check refuses the mount at `shared`
                24 |         "image": image("judge/Dockerfile", mount = SHARED, target = stage, args = BASE_ARGS),
                   |                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `_judge()`, called at lib/images.star:44:15
                in `images_task()`, called at bad/refused/task.star:5:1
            lib/images.star:24:18: the stand-in check refuses `judge/Dockerfile`
                24 |         "image": image("judge/Dockerfile", mount = SHARED, target = stage, args = BASE_ARGS),
                   |                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `_judge()`, called at lib/images.star:44:47
                in `images_task()`, called at bad/refused/task.star:5:1
            lib/images.star:24:18: the stand-in check refuses the mount at `shared`
                24 |         "image": image("judge/Dockerfile", mount = SHARED, target = stage, args = BASE_ARGS),
                   |                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `_judge()`, called at lib/images.star:44:47
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
            lib/images.star:38:13: building the image failed: the stand-in build fails `tools/Dockerfile`
                38 |     tools = image("tools/Dockerfile", args = BASE_ARGS)
                   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `images_task()`, called at bad/broken/task.star:5:1
            lib/images.star:39:17: building the image failed: the stand-in build fails `toolchain/Dockerfile`
                39 |     toolchain = image("toolchain/Dockerfile", target = "Go")
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
        r#"fail(", ".join(codex(image("Dockerfile", context = "//images"), model = "model-small", effort = "high", tools = "native", workspace = "/workspace", prompt = "", deadline_seconds = 60, finalize_grace_seconds = 0).keys()))"#,
        &expect![[r#"
            text.star:1:1: fail: image, runtime, egress
                1 | fail(", ".join(codex(image("Dockerfile", context = "//images"), model = "model-small", effort = "high", tools = "native", workspace = "/workspace", prompt = "", deadline_seconds = 60, finalize_grace_seconds = 0).keys()))
                  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"
A = image("Dockerfile", context = "//images")
B = codex(A, model = "model-small", effort = "high", tools = "native", workspace = "/workspace", prompt = "", deadline_seconds = 60, finalize_grace_seconds = 0)
C = codex(A, model = "model-large", effort = "low", tools = "native", workspace = "/home/agent", prompt = "Solve it.", deadline_seconds = 60, finalize_grace_seconds = 0)
fail(str(int(B["image"] == C["image"])))"#,
        &expect![[r#"
            text.star:4:1: fail: 1
                4 | fail(str(int(B["image"] == C["image"])))
                  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
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
X = codex(1, model = "m", effort = "high", tools = "native", workspace = "/w", prompt = "", deadline_seconds = 60, finalize_grace_seconds = 0)"#,
        &expect![[r#"
            lib.star:1:5: `codex()` is called while `//lib.star` loads: call it in the entry file, which loads `//lib.star`
                1 | X = codex(1, model = "m", effort = "high", tools = "native", workspace = "/w", prompt = "", deadline_seconds = 60, finalize_grace_seconds = 0)
                  |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
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
        r#"codex(image("Dockerfile", context = "//images"), model = "model-small", effort = "hard", tools = "native", workspace = "/workspace", prompt = "", deadline_seconds = 60, finalize_grace_seconds = 0)"#,
        &expect![[r#"
            text.star:1:82: `hard` is not a reasoning effort: use one of none, minimal, low, medium, high, xhigh, max, ultra
                1 | codex(image("Dockerfile", context = "//images"), model = "model-small", effort = "hard", tools = "native", workspace = "/workspace", prompt = "", deadline_seconds = 60, finalize_grace_seconds = 0)
                  |                                                                                  ^^^^^^"#]],
    );
    check(
        r#"codex(image("Dockerfile", context = "//images"), model = "model-small", effort = "high", tools = "both", workspace = "/workspace", prompt = "", deadline_seconds = 60, finalize_grace_seconds = 0)"#,
        &expect![[r#"
            text.star:1:98: `both` is not a way to send Codex's tools: use one of native, functions
                1 | codex(image("Dockerfile", context = "//images"), model = "model-small", effort = "high", tools = "both", workspace = "/workspace", prompt = "", deadline_seconds = 60, finalize_grace_seconds = 0)
                  |                                                                                                  ^^^^^^"#]],
    );
    check(
        r#"codex(image("Dockerfile", context = "//images"), model = "m", effort = "high", tools = "native", workspace = "/w", prompt = "", deadline_seconds = 0, finalize_grace_seconds = 0)"#,
        &expect![[r#"
            text.star:1:148: `codex`'s `deadline_seconds` is 0: give a positive int
                1 | codex(image("Dockerfile", context = "//images"), model = "m", effort = "high", tools = "native", workspace = "/w", prompt = "", deadline_seconds = 0, finalize_grace_seconds = 0)
                  |                                                                                                                                                    ^"#]],
    );
    check(
        r#"codex(image("Dockerfile", context = "//images"), model = "m", effort = "high", tools = "native", workspace = "/w", prompt = "", deadline_seconds = "1h", finalize_grace_seconds = 0)"#,
        &expect![[r#"
            text.star:1:148: `codex`'s `deadline_seconds` can't be `"1h"`: give an int
                1 | codex(image("Dockerfile", context = "//images"), model = "m", effort = "high", tools = "native", workspace = "/w", prompt = "", deadline_seconds = "1h", finalize_grace_seconds = 0)
                  |                                                                                                                                                    ^^^^"#]],
    );
    check(
        r#"codex(image("Dockerfile", context = "//images"), model = "m", effort = "high", tools = "native", workspace = "/w", prompt = "", deadline_seconds = 60, finalize_grace_seconds = -1)"#,
        &expect![[r#"
            text.star:1:177: `codex`'s `finalize_grace_seconds` is -1: give an int of 0 or more
                1 | codex(image("Dockerfile", context = "//images"), model = "m", effort = "high", tools = "native", workspace = "/w", prompt = "", deadline_seconds = 60, finalize_grace_seconds = -1)
                  |                                                                                                                                                                                 ^^"#]],
    );
    check(
        r#"codex(image("Dockerfile", context = "//images"), model = "model-small", effort = "high", tools = "functions", workspace = "/workspace", prompt = "", deadline_seconds = 60, finalize_grace_seconds = 0)"#,
        &expect![[r#"
            text.star:1:58: Codex's catalog entry for `model-small` isn't carried, and sending its tools as functions needs it: use `gpt-6.1-terra` or `glm-5.3`
                1 | codex(image("Dockerfile", context = "//images"), model = "model-small", effort = "high", tools = "functions", workspace = "/workspace", prompt = "", deadline_seconds = 60, finalize_grace_seconds = 0)
                  |                                                          ^^^^^^^^^^^^^"#]],
    );
    check(
        r#"codex(image("Dockerfile", context = "//images"), model = "model-small", effort = "high", tools = "native", workspace = "/pumpkin/home", prompt = "", deadline_seconds = 60, finalize_grace_seconds = 0)"#,
        &expect![[r#"
            text.star:1:120: `/pumpkin/home/.codex` is in `/pumpkin`, which holds Pumpkin's own files: name a path outside it
                1 | codex(image("Dockerfile", context = "//images"), model = "model-small", effort = "high", tools = "native", workspace = "/pumpkin/home", prompt = "", deadline_seconds = 60, finalize_grace_seconds = 0)
                  |                                                                                                                        ^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"codex(image("Dockerfile", context = "//images"), model = "model-small", effort = "high", tools = "native", workspace = "workspace", prompt = "", deadline_seconds = 60, finalize_grace_seconds = 0)"#,
        &expect![[r#"
            text.star:1:120: `workspace` is not a normalized absolute path: start with `/`, and use no empty, `.` or `..` components and no NUL byte
                1 | codex(image("Dockerfile", context = "//images"), model = "model-small", effort = "high", tools = "native", workspace = "workspace", prompt = "", deadline_seconds = 60, finalize_grace_seconds = 0)
                  |                                                                                                                        ^^^^^^^^^^^"#]],
    );
    check(
        r#"codex(image("Dockerfile", context = "//images"), model = "model small", effort = "high", tools = "native", workspace = "/workspace", prompt = "", deadline_seconds = 60, finalize_grace_seconds = 0)"#,
        &expect![[r#"
            text.star:1:58: `model small` is not a model name: use one word
                1 | codex(image("Dockerfile", context = "//images"), model = "model small", effort = "high", tools = "native", workspace = "/workspace", prompt = "", deadline_seconds = 60, finalize_grace_seconds = 0)
                  |                                                          ^^^^^^^^^^^^^"#]],
    );
    check(
        r#"codex("Dockerfile", model = "model-small", effort = "high", tools = "native", workspace = "/workspace", prompt = "", deadline_seconds = 60, finalize_grace_seconds = 0)"#,
        &expect![[r#"
            text.star:1:7: `codex`'s `base` can't be `"Dockerfile"`: give an image, as `image()` makes
                1 | codex("Dockerfile", model = "model-small", effort = "high", tools = "native", workspace = "/workspace", prompt = "", deadline_seconds = 60, finalize_grace_seconds = 0)
                  |       ^^^^^^^^^^^^"#]],
    );
    check(
        r#"codex(image("Dockerfile", context = "//images"), model = "m")"#,
        &expect![[r#"
            text.star:1:1: `codex` lacks `effort`, `tools`, `workspace`, `prompt`, `deadline_seconds`, `finalize_grace_seconds`: give them
                1 | codex(image("Dockerfile", context = "//images"), model = "m")
                  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
    );
    check(
        r#"codex(image("Dockerfile", context = "//" + "/".join(["abcdefghij" for a in [1, 2] for b in [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]])), model = "model-small", effort = "high", tools = "native", workspace = "/workspace", prompt = "", deadline_seconds = 60, finalize_grace_seconds = 0)"#,
        &expect![[r#"
            text.star:1:7: the image's repository, `pumpkin/abcdefghij/abcdefghij/abcdefghij/abcdefghij/abcdefgh…`, has 263 characters, more than Docker's 255: name the directory of the recipe with fewer
                1 | codex(image("Dockerfile", context = "//" + "/".join(["abcdefghij" for a in [1, 2] for b in [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]])), model = "model-small", effort = "high", tools = "native", workspace = "/workspace", prompt = "", deadline_seconds = 60, finalize_grace_seconds = 0)
                  |       ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^"#]],
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
workflow(name = "neutral/twice", roles = [], score = None)
workflow(name = "neutral/twice", roles = [], score = None)"#,
        &expect![[r#"
            text.star:2:1: `workflow()` is called a second time: call it once
                2 | workflow(name = "neutral/twice", roles = [], score = None)
                  | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                it is first called at text.star:1:1"#]],
    );
}
