//! The vendored Starlark suites of `tests/conformance`, run through `lang::evaluate` for what the
//! subset does with each program: it runs it, refuses it, or means something other than the
//! suite says, by running where an error is expected or by failing an assertion. That last is
//! `Wrong`, the one failure, since the subset only refuses; the snapshot pins that none is left,
//! and how many runs of each suite have each outcome.
//!
//! A file is split into chunks at its lines of `---`, each a program. A line with `###` marks
//! an error that the program must give; in `spec`, the tags `go:`, `java:` and `rust:` after it
//! say which implementations give it, and the rest of the suites take every `###` as an error.
//! Each `assert.fails(…)` statement of a chunk is a run of its own, that must err, and the run of
//! the whole chunk leaves those statements out. The suites call `assert.eq` and the like, which
//! the language has no module for, so each call is renamed to the function of `ASSERT`, a file
//! loaded in place of the suites' own.

use std::collections::BTreeMap;
use std::path::Path;

use expect_test::{Expect, expect};
use lang::{Args, Bounds};

use crate::{Case, files, on_stack};

// What `fail` says in `ASSERT`. A run whose error holds it ended in a failed assertion, not in a
// refusal.
const FAILED: &str = "the conformance assertion fails";

// Each assertion fails with `_FAILED`, bound to a name, so that no source line that a problem
// shows holds `FAILED` but the binding's.
const ASSERT: &str = r#"
_FAILED = "the conformance assertion fails"

def assert_eq(x, y):
    if x != y:
        fail(_FAILED)

def assert_ne(x, y):
    if x == y:
        fail(_FAILED)

def assert_true(cond, msg = None):
    if not cond:
        fail(_FAILED)

def assert_(cond, msg = None):
    if not cond:
        fail(_FAILED)

def assert_lt(x, y):
    if not (x < y):
        fail(_FAILED)

def assert_contains(x, y):
    if y not in x:
        fail(_FAILED)

def assert_fail(msg):
    fail(_FAILED)

# Only an error in `f()` ends the evaluation before `fail` does.
def assert_fails(f, pattern):
    f()
    fail(_FAILED)

def freeze(x):
    return x
"#;

// Loads a chunk, `//case.star`, then gives a whole workflow, so that its evaluation succeeds
// unless the chunk's problem ends it.
const ENTRY: &str = r#"load("//case.star", "conformance_done")
load("//lib/parts.star", "AGENT", "FINAL_TARGET", "FINAL")

workflow(name = "conformance/chunk", roles = [AGENT, FINAL_TARGET], final = FINAL, deadline_seconds = 1)
"#;

#[derive(Clone, Copy)]
enum Expected {
    Runs,
    Errs,
    Either,
}

// A chunk whose one assertion fails, so that its run must be `Wrong`: if a change to how `fail`
// shows its message hid the assertions' failures, the snapshot would show it here.
const CANARY: &str = "assert.eq(1, 2)\n";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Outcome {
    Runs,
    Errs,
    Refused,
    Wrong,
}

impl Outcome {
    fn name(self) -> &'static str {
        match self {
            Self::Runs => "run",
            Self::Errs => "err as expected",
            Self::Refused => "refused or err",
            Self::Wrong => "WRONG",
        }
    }
}

// Each suite is named, and the directory of `tests/conformance` it runs.
fn check(suites: &[(&str, &str)], expect: &Expect) {
    let fixtures = files();
    let shown = on_stack(|| {
        let args = Args::parse([]).unwrap();
        let mut shown = Vec::new();
        let mut wrong = Vec::new();
        for run in runs("bazel", CANARY) {
            let outcome = evaluate(&run, &fixtures, &args);
            shown.push(format!(
                "the canary `{}`: {}\n",
                CANARY.trim_end(),
                outcome.name()
            ));
        }
        for &(suite, dir) in suites {
            let mut count = BTreeMap::<Outcome, usize>::new();
            let below = crate::files_below(&format!("tests/conformance/{dir}"));
            let star = |path: &&String| Path::new(path).extension() == Some("star".as_ref());
            for (path, text) in below.iter().filter(|(path, _)| star(path)) {
                for (first, chunk) in chunks(text) {
                    for run in runs(dir, chunk) {
                        let line = first + run.line;
                        let outcome = evaluate(&run, &fixtures, &args);
                        *count.entry(outcome).or_default() += 1;
                        if outcome == Outcome::Wrong {
                            wrong.push(format!("{suite}: {dir}/{path}:{line}"));
                        }
                    }
                }
            }
            let outcomes = [
                Outcome::Runs,
                Outcome::Errs,
                Outcome::Refused,
                Outcome::Wrong,
            ];
            let counts = outcomes.map(|outcome| {
                let n = count.get(&outcome).copied().unwrap_or(0);
                format!("{n} {}", outcome.name())
            });
            shown.push(format!("{suite}: {}", counts.join(", ")));
        }
        shown.push(String::from("\nWRONG:"));
        if wrong.is_empty() {
            wrong.push(String::from("none"));
        }
        shown.extend(wrong);
        shown.join("\n")
    });
    expect.assert_eq(&shown);
}

fn evaluate(run: &Run, fixtures: &BTreeMap<String, String>, args: &Args) -> Outcome {
    let Run { expected, case, .. } = run;
    let entry = Case {
        entry: "entry.star",
        held: vec![
            ("entry.star", ENTRY),
            ("case.star", case.as_str()),
            ("assert.star", ASSERT),
        ],
        files: fixtures,
        bounds: Bounds::WORKFLOW,
    };
    let evaluated = entry.evaluate(args).map(|_| ());
    match (&evaluated, *expected) {
        (Ok(()), Expected::Errs) => Outcome::Wrong,
        (Ok(()), Expected::Runs | Expected::Either) => Outcome::Runs,
        (Err(shown), _) if shown.contains(FAILED) => Outcome::Wrong,
        (Err(_), Expected::Errs) => Outcome::Errs,
        (Err(_), Expected::Runs | Expected::Either) => Outcome::Refused,
    }
}

// Each chunk with the line it starts at, counting from 1.
fn chunks(text: &str) -> Vec<(usize, &str)> {
    let mut chunks = Vec::new();
    let (mut start, mut first) = (0, 1);
    let mut at = 0;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        if line.trim_end() == "---" {
            chunks.push((first, &text[start..at]));
            start = at + line.len();
            first = index + 2;
        }
        at += line.len();
    }
    chunks.push((first, &text[start..]));
    chunks
}

struct Run {
    expected: Expected,
    /// The line of the chunk that names the run, counted from 0.
    line: usize,
    case: String,
}

// An `assert.fails` statement ends the evaluation when it passes, as the language can't catch the
// error that `f()` raises, so each gets a run of its own. Statements are told apart line by
// line, by `depths`, which reads no string across lines: a triple-quoted string or a `\` that
// joins lines at the top level could cut a statement, and its run would err however the whole
// reads.
fn runs(dir: &str, chunk: &str) -> Vec<Run> {
    // Whether a line expects an error of every implementation, and which implementations a
    // tagged line expects one of.
    let (mut every, mut tagged) = (false, [false; 3]);
    let mut lines = Vec::new();
    for line in chunk.lines() {
        if let Some((_, after)) = line.split_once("###") {
            let tag = ["go:", "java:", "rust:"]
                .iter()
                .position(|tag| dir == "spec" && after.trim_start().starts_with(tag));
            match tag {
                Some(tag) => tagged[tag] = true,
                None => every = true,
            }
        }
        // `case` loads the assertion functions itself, so the suite's own load of them goes. A
        // load that `###` marks is a case of its own, of a load's errors.
        let assert_load = ["load(\"assert.star\"", "load('assert.star'"]
            .iter()
            .any(|load| line.starts_with(load));
        if assert_load && !line.contains("###") {
            lines.push("");
        } else {
            lines.push(line);
        }
    }
    let [go, java, rust] = tagged;
    // Bazel's Starlark, which is Java's, is the standard: an error it expects is expected.
    let expected = if every || java {
        Expected::Errs
    } else if go || rust {
        Expected::Either
    } else {
        Expected::Runs
    };
    let fails = fails_statements(&lines);
    // The lines up to `end`, each statement of `fails` but `kept` blanked.
    let blanked = |end: usize, kept: Option<usize>| -> Vec<String> {
        (lines.iter().enumerate().take(end))
            .map(|(at, line)| {
                let left_out = (fails.iter().enumerate())
                    .any(|(index, range)| range.contains(&at) && Some(index) != kept);
                if left_out {
                    String::new()
                } else {
                    (*line).to_owned()
                }
            })
            .collect()
    };
    // Where starlark-go's and Bazel's suites expect an error at a top-level statement, nothing
    // after it runs upstream, so the run ends there, and the error must be that statement's. In
    // `spec`, a `###` line may stand above the code it expects an error of, so the run goes on.
    let first_error = (dir != "spec")
        .then(|| lines.iter().position(|line| line.contains("###")))
        .flatten();
    let end = first_error
        .and_then(|at| statement_end(&lines, &depths(&lines), at))
        .unwrap_or(lines.len());
    let mut runs = vec![Run {
        expected,
        line: 0,
        case: case(&blanked(end, None)),
    }];
    runs.extend(fails.iter().enumerate().map(|(index, range)| Run {
        expected: Expected::Errs,
        line: range.start,
        case: case(&blanked(range.end, Some(index))),
    }));
    runs
}

// The bracket depth at the start of each line, and after the last, outside strings and comments:
// a statement starts at a line whose depth is 0 and that isn't indented.
fn depths(lines: &[&str]) -> Vec<usize> {
    let mut depths = Vec::with_capacity(lines.len() + 1);
    let mut depth = 0_usize;
    for line in lines {
        depths.push(depth);
        let mut quote = None;
        let mut chars = line.chars();
        while let Some(char) = chars.next() {
            match (quote, char) {
                (Some(_), '\\') => {
                    chars.next();
                }
                (Some(open), _) if char == open => quote = None,
                (None, '\'' | '"') => quote = Some(char),
                (None, '(' | '[' | '{') => depth += 1,
                (None, ')' | ']' | '}') => depth = depth.saturating_sub(1),
                (None, '#') => break,
                (Some(_) | None, _) => {}
            }
        }
    }
    depths.push(depth);
    depths
}

fn starts_statement(lines: &[&str], depths: &[usize], at: usize) -> bool {
    depths[at] == 0 && lines[at].starts_with(|char: char| !char.is_whitespace())
}

// The end of the top-level statement that the line at `at` is in, if it is in one: where the next
// one starts, or the end of the lines.
fn statement_end(lines: &[&str], depths: &[usize], at: usize) -> Option<usize> {
    let start = (0..=at).rev().find(|&line| depths[line] == 0)?;
    if !starts_statement(lines, depths, start) {
        return None;
    }
    let next = (at + 1..lines.len()).find(|&line| starts_statement(lines, depths, line));
    Some(next.unwrap_or(lines.len()))
}

fn fails_statements(lines: &[&str]) -> Vec<std::ops::Range<usize>> {
    let depths = depths(lines);
    (0..lines.len())
        .filter(|&at| {
            starts_statement(lines, &depths, at)
                && ["assert.fails(", "assert_fails("]
                    .iter()
                    .any(|call| lines[at].starts_with(call))
        })
        .filter_map(|at| Some(at..statement_end(lines, &depths, at)?))
        .collect()
}

fn case(lines: &[String]) -> String {
    let mut body = lines.join("\n");
    for name in ["eq", "ne", "true", "lt", "contains", "fail", "fails"] {
        body = body.replace(&format!("assert.{name}("), &format!("assert_{name}("));
    }
    let names = [
        "assert_eq",
        "assert_ne",
        "assert_true",
        "assert_",
        "assert_lt",
        "assert_contains",
        "assert_fail",
        "assert_fails",
        "freeze",
    ];
    let used: Vec<String> = (names.iter())
        .filter(|name| names_used(&body, name))
        .map(|name| format!("\"{name}\""))
        .collect();
    let load = if used.is_empty() {
        String::new()
    } else {
        format!("load(\"//assert.star\", {})", used.join(", "))
    };
    format!("{load}\n{body}\nconformance_done = True\n")
}

fn names_used(text: &str, name: &str) -> bool {
    let word = |char: char| char.is_ascii_alphanumeric() || char == '_';
    text.match_indices(name).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + name.len()..].chars().next();
        !before.is_some_and(word) && !after.is_some_and(word)
    })
}

#[test]
fn suites() {
    check(
        &[
            ("spec", "spec"),
            ("starlark-go", "starlark-go/starlark"),
            ("starlark-go resolve", "starlark-go/resolve"),
            ("starlark-go syntax", "starlark-go/syntax"),
            ("bazel", "bazel"),
        ],
        &expect![[r#"
            the canary `assert.eq(1, 2)`: WRONG

            spec: 99 run, 242 err as expected, 89 refused or err, 0 WRONG
            starlark-go: 9 run, 330 err as expected, 43 refused or err, 0 WRONG
            starlark-go resolve: 2 run, 29 err as expected, 13 refused or err, 0 WRONG
            starlark-go syntax: 3 run, 39 err as expected, 10 refused or err, 0 WRONG
            bazel: 7 run, 318 err as expected, 45 refused or err, 0 WRONG

            WRONG:
            none"#]],
    );
}
