//! The vendored Starlark suites of `tests/conformance`, run through `lang::evaluate` for what the
//! subset does with each program: it runs it, refuses it, or means something other than the
//! suite says, by running where an error is expected or by failing an assertion. That last is
//! `Wrong`, the one failure, since the subset only refuses; the snapshot pins that none is left,
//! how many runs of each suite have each outcome, and by how many runs each problem is met.
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
load("//lib/parts.star", "AGENT", "JUDGE", "SCORE")

workflow(name = "conformance/chunk", roles = [AGENT, JUDGE], score = SCORE)
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
        let mut problems = BTreeMap::<String, usize>::new();
        for run in runs("bazel", CANARY) {
            let (outcome, _) = evaluate(&run, &fixtures, &args);
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
                        let (outcome, evaluated) = evaluate(&run, &fixtures, &args);
                        *count.entry(outcome).or_default() += 1;
                        if outcome == Outcome::Wrong {
                            wrong.push(format!("{suite}: {dir}/{path}:{line}"));
                        } else if let Err(problem) = evaluated {
                            *problems.entry(message(&problem)).or_default() += 1;
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
        shown.push(String::from("\nthe problems runs end with, by how many:"));
        let mut problems: Vec<(usize, String)> =
            problems.into_iter().map(|(text, n)| (n, text)).collect();
        problems.sort_by(|(a, a_text), (b, b_text)| b.cmp(a).then(a_text.cmp(b_text)));
        shown.extend(problems.iter().map(|(n, text)| format!("{n:>4} {text}")));
        shown.join("\n")
    });
    expect.assert_eq(&shown);
}

fn evaluate(
    run: &Run,
    fixtures: &BTreeMap<String, String>,
    args: &Args,
) -> (Outcome, Result<(), String>) {
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
    let outcome = match (&evaluated, *expected) {
        (Ok(()), Expected::Errs) => Outcome::Wrong,
        (Ok(()), Expected::Runs | Expected::Either) => Outcome::Runs,
        (Err(shown), _) if shown.contains(FAILED) => Outcome::Wrong,
        (Err(_), Expected::Errs) => Outcome::Errs,
        (Err(_), Expected::Runs | Expected::Either) => Outcome::Refused,
    };
    (outcome, evaluated)
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

// A problem without its file and position, so that the same problem counts as one.
fn message(shown: &str) -> String {
    let first = shown.lines().next().unwrap_or_default();
    let (_, rest) = first.split_once(".star").unwrap_or(("", first));
    let rest = rest.trim_start_matches(|char: char| char == ':' || char.is_ascii_digit());
    rest.trim_start().to_owned()
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the snapshot")]
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
            starlark-go: 8 run, 330 err as expected, 44 refused or err, 0 WRONG
            starlark-go resolve: 1 run, 29 err as expected, 14 refused or err, 0 WRONG
            starlark-go syntax: 3 run, 39 err as expected, 10 refused or err, 0 WRONG
            bazel: 7 run, 318 err as expected, 45 refused or err, 0 WRONG

            WRONG:
            none

            the problems runs end with, by how many:
             189 `lambda` is not in Pumpkin's Starlark: write a `def` at a library's top level, and pass its name
             134 a slice is not in Pumpkin's Starlark: index the elements one by one
              93 `0.0`, a float, is not in Pumpkin's Starlark: write an int
              71 assigning to an element, which would change a value, is not in Pumpkin's Starlark: make a new value, as `d | {k: v}` makes a dict
              42 `&`, a bitwise operator, is not in Pumpkin's Starlark: write the int it gives
              36 unary `+` is not in Pumpkin's Starlark: write the int alone
              30 `<<`, a bitwise operator, is not in Pumpkin's Starlark: write the int it gives
              30 `int` with 2 arguments is not in Pumpkin's Starlark: give it 1 argument
              26 `2.0`, a float, is not in Pumpkin's Starlark: write an int
              22 a `for` statement is not in Pumpkin's Starlark: build the list with a comprehension, as `[f(x) for x in xs]`
              15 a `def` inside a function is not in Pumpkin's Starlark: write it at the top level, passing it what it uses
              14 `\x`, an escape that some Starlark implementations lack, is not in Pumpkin's Starlark: write the character itself, or an ASCII one in octal, as `\101` for `A`
              13 `*args` is not in Pumpkin's Starlark: take the values in a list
              13 `+=` is not in Pumpkin's Starlark: bind the result with `=`
              11 `dict`, a builtin of Starlark's, is not in Pumpkin's Starlark: write the value out, or build it with a comprehension
              10 `list`, a builtin of Starlark's, is not in Pumpkin's Starlark: write the value out, or build it with a comprehension
               8 `append`, which would change the list, is not in Pumpkin's Starlark: make a new list with `+`, as values never change
               8 indexing a string, whose elements Starlark's implementations count differently, is not in Pumpkin's Starlark: split it, or test it with `startswith`
               7 `module` is no label: start it with `//`, the root
               7 this `format` string has a `}` that closes no field: write `}}` for a `}`
               6 `%` of a string, which formats it, is not in Pumpkin's Starlark: format it with `format`, or join strings with `+`
               6 `M` is not defined: bind it in this file, or load it
               6 `all`, a builtin of Starlark's, is not in Pumpkin's Starlark: compute the value with `bool`, `int`, `len`, `sorted` and `str`
               6 `min`, a builtin of Starlark's, is not in Pumpkin's Starlark: compute the value with `bool`, `int`, `len`, `sorted` and `str`
               6 `range`, a builtin of Starlark's, is not in Pumpkin's Starlark: write the value out, or build it with a comprehension
               6 `type`, a builtin of Starlark's, is not in Pumpkin's Starlark: compute the value with `bool`, `int`, `len`, `sorted` and `str`
               5 `*` before an argument, which passes a sequence's elements, is not in Pumpkin's Starlark: pass each argument by itself
               5 `endswith` of a tuple is not in Pumpkin's Starlark: give it a string
               5 `index` of a list is not in Pumpkin's Starlark: give it to a function, as `len`
               5 `pop`, which would change the list, is not in Pumpkin's Starlark: make a new list with `+`, as values never change
               5 `splitlines` of a string is not in Pumpkin's Starlark: call one of `endswith`, `format`, `join`, `replace`, `split`, `startswith`
               5 `startswith` of a tuple is not in Pumpkin's Starlark: give it a string
               5 a list can't be a dict's key: key a dict with strings or ints
               5 a tab in indentation is not in Pumpkin's Starlark: indent with spaces only
               5 a tuple of 2 values can't be unpacked into 3 names: give a list or a tuple of 3
               5 a tuple of 3 values can't be unpacked into 2 names: give a list or a tuple of 2
               5 this `format` string has numbered fields and `{}` fields: number every field, or none
               4 `**kwargs`, which would let a misspelt keyword pass unseen, is not in Pumpkin's Starlark: name each parameter
               4 `a` is no label: start it with `//`, the root
               4 `assert` is reserved in Starlark: use another word
               4 `extend`, which would change the list, is not in Pumpkin's Starlark: make a new list with `+`, as values never change
               4 `partition` of a string is not in Pumpkin's Starlark: call one of `endswith`, `format`, `join`, `replace`, `split`, `startswith`
               4 `reversed`, a builtin of Starlark's, is not in Pumpkin's Starlark: write the value out, or build it with a comprehension
               4 `while` is not in Starlark: build the list with a comprehension, as `[f(x) for x in xs]`
               4 a `for` statement at a file's top level is not in Starlark: build the list with a comprehension, as `[f(x) for x in xs]`
               4 an int can't be unpacked into names: give a list or a tuple of 2
               3 `*` of a sequence and an int, which would let an expression make a value of any size, is not in Pumpkin's Starlark: write the elements out, or join with `+`
               3 `1 // 0` divides by zero: divide by another int
               3 `any`, a builtin of Starlark's, is not in Pumpkin's Starlark: compute the value with `bool`, `int`, `len`, `sorted` and `str`
               3 `assert.star` is no label: start it with `//`, the root
               3 `tuple`, a builtin of Starlark's, is not in Pumpkin's Starlark: write the value out, or build it with a comprehension
               3 `{ {}` is no field of a `format` string: write `{}`, `{0}` or `{name}`
               3 `{0,1}` is no field of a `format` string: write `{}`, `{0}` or `{name}`
               3 `{}` names no argument of this `format` call, which has 1 by position: give it one more
               3 a method named but not called is not in Pumpkin's Starlark: call it here, as `.clear(…)`
               3 a method named but not called is not in Pumpkin's Starlark: call it here, as `.x(…)`
               3 a positional argument follows a keyword argument: put positional arguments first
               3 a tuple of 0 values can't be unpacked into 2 names: give a list or a tuple of 2
               3 a tuple of 1 value can't be unpacked into 2 names: give a list or a tuple of 2
               3 an index of several values is not in Pumpkin's Starlark: index with one value
               3 an int can't be unpacked into names: give a list or a tuple of 0
               3 an int can't be unpacked into names: give a list or a tuple of 1
               3 an int can't be unpacked into names: give a list or a tuple of 3
               3 comparisons don't chain in Starlark: join them with `and`
               2 `**` before an argument, which passes a dict's entries, is not in Pumpkin's Starlark: name each keyword argument
               2 `3` indexes no element of a list of 3: index from -3 to 2
               2 `M` is bound already: bind each global once
               2 `` is no label: start it with `//`, the root
               2 `dir`, a builtin of Starlark's, is not in Pumpkin's Starlark: compute the value with `bool`, `int`, `len`, `sorted` and `str`
               2 `enumerate`, a builtin of Starlark's, is not in Pumpkin's Starlark: write the value out, or build it with a comprehension
               2 `find` of a string is not in Pumpkin's Starlark: call one of `endswith`, `format`, `join`, `replace`, `split`, `startswith`
               2 `g` is used before it is bound: bind it above this use
               2 `getattr`, a builtin of Starlark's, is not in Pumpkin's Starlark: compute the value with `bool`, `int`, `len`, `sorted` and `str`
               2 `in` can't find an int in a string: look for a string in it
               2 `int` can't read `"1.5"`: give it decimal digits, after `-` or `+` or neither
               2 `int` can't read `"ab"`: give it decimal digits, after `-` or `+` or neither
               2 `int` can't take `None`: give it an int, a bool or a string of digits
               2 `int`'s `base` is not in Pumpkin's Starlark: give `int` 1 argument, by position
               2 `isalnum` of a string is not in Pumpkin's Starlark: call one of `endswith`, `format`, `join`, `replace`, `split`, `startswith`
               2 `lower` of a string is not in Pumpkin's Starlark: call one of `endswith`, `format`, `join`, `replace`, `split`, `startswith`
               2 `max`, a builtin of Starlark's, is not in Pumpkin's Starlark: compute the value with `bool`, `int`, `len`, `sorted` and `str`
               2 `remove`, which would change the list, is not in Pumpkin's Starlark: make a new list with `+`, as values never change
               2 `rpartition` of a string is not in Pumpkin's Starlark: call one of `endswith`, `format`, `join`, `replace`, `split`, `startswith`
               2 `sorted` of a list of mixed or other values than ints or strings is not in Pumpkin's Starlark: give it only ints, or only strings
               2 `split` with 2 arguments is not in Pumpkin's Starlark: give it 1 argument
               2 `x` is bound already: bind each global once
               2 `x` is not defined: bind it in this file, or load it
               2 `x` is used before it is bound: bind it above this use
               2 `zip`, a builtin of Starlark's, is not in Pumpkin's Starlark: write the value out, or build it with a comprehension
               2 `{0.1}` is no field of a `format` string: write `{}`, `{0}` or `{name}`
               2 `{0}` names no argument of this `format` call, which has 0 by position: give it one more
               2 `{1}` names no argument of this `format` call, which has 1 by position: give it one more
               2 `{2}` names no argument of this `format` call, which has 2 by position: give it one more
               2 `{b}` names no keyword argument of this `format` call: give it `b = …`
               2 `{test,}` is no field of a `format` string: write `{}`, `{0}` or `{name}`
               2 `{test.}` is no field of a `format` string: write `{}`, `{0}` or `{name}`
               2 `{test[}` is no field of a `format` string: write `{}`, `{0}` or `{name}`
               2 `{}` names no argument of this `format` call, which has 0 by position: give it one more
               2 `|=` is not in Pumpkin's Starlark: bind the result with `=`
               2 a list of 3 values can't be unpacked into 2 names: give a list or a tuple of 2
               2 a tuple of 1 value can't be unpacked into 0 names: give a list or a tuple of 0
               2 a tuple without parentheses can't end with `,`: drop the `,`, or put the tuple in parentheses
               2 fail: 1
               2 unexpected `else`: expected `]`
               2 unexpected end of line: expected an expression
               1 `"a"` is no key of this dict: index it with one of its keys, or give a default with `get`
               1 `"one"` is a key of this dict already: give each key once
               1 `*` can't take a string and a bool: give it two ints
               1 `+` can't take a bool and an int: give it two ints, two strings, two lists or two tuples
               1 `+` can't take a list and a tuple: give it two ints, two strings, two lists or two tuples
               1 `+` can't take a list and an int: give it two ints, two strings, two lists or two tuples
               1 `+` can't take a string and an int: give it two ints, two strings, two lists or two tuples
               1 `+` can't take a tuple and a list: give it two ints, two strings, two lists or two tuples
               1 `+` can't take a tuple and an int: give it two ints, two strings, two lists or two tuples
               1 `-2` indexes no element of a list of 1: index from -1 to 0
               1 `-4` indexes no element of a list of 3: index from -3 to 2
               1 `/`, which divides into a float, is not in Pumpkin's Starlark: divide with `//`
               1 `0in`, a number and then `in`, is not in Pumpkin's Starlark: put a space between them
               1 `1 % 0` divides by zero: divide by another int
               1 `1` indexes no element of a list of 1: index from -1 to 0
               1 `1` indexes no element of a list that is empty: index one that has some
               1 `6burgle` is no number: write digits of its base only, and a space between a number and a name
               1 `<` can't take `None` and a bool: give it two ints or two strings
               1 `<` can't take `None` and an int: give it two ints or two strings
               1 `<` can't take a dict and `None`: give it two ints or two strings
               1 `<` can't take a list and a string: give it two ints or two strings
               1 `<` can't take a string and a tuple: give it two ints or two strings
               1 `<` can't take an int and a list: give it two ints or two strings
               1 `<` of two bools is not in Pumpkin's Starlark: order two ints or two strings
               1 `<` of two lists is not in Pumpkin's Starlark: order two ints or two strings
               1 `StartsWith` is no method of a string: call one of `endswith`, `format`, `join`, `replace`, `split`, `startswith`
               1 `U` is not defined: bind it in this file, or load it
               1 `U` is used before it is bound: bind it above this use
               1 `a` is a parameter already: name each once
               1 `abc` is not defined: bind it in this file, or load it
               1 `bool` with no argument is not in Pumpkin's Starlark: give it 1 argument
               1 `break` stands only in a loop's body, and no loop holds it: filter a comprehension with `if`
               1 `clear`, which would change the dict, is not in Pumpkin's Starlark: make a new dict with `|`, as values never change
               1 `clear`, which would change the list, is not in Pumpkin's Starlark: make a new list with `+`, as values never change
               1 `f` lacks `mm`: give it
               1 `fail` with 3 arguments is not in Pumpkin's Starlark: give it 1 argument
               1 `fail` with no argument is not in Pumpkin's Starlark: give it 1 argument
               1 `fail`'s `sep` is not in Pumpkin's Starlark: give `fail` 1 argument, by position
               1 `fin` is no method of a string: call one of `endswith`, `format`, `join`, `replace`, `split`, `startswith`
               1 `get`'s `default` by name is not in Pumpkin's Starlark: give it by position, after the key
               1 `hash`, a builtin of Starlark's, is not in Pumpkin's Starlark: compute the value with `bool`, `int`, `len`, `sorted` and `str`
               1 `in` can't look in an int: give it a list, a tuple, a dict or a string to look in
               1 `index` of a string is not in Pumpkin's Starlark: call one of `endswith`, `format`, `join`, `replace`, `split`, `startswith`
               1 `insert`, which would change the list, is not in Pumpkin's Starlark: make a new list with `+`, as values never change
               1 `int` can't read `"  42  "`: give it decimal digits, after `-` or `+` or neither
               1 `int` can't read `""`: give it decimal digits, after `-` or `+` or neither
               1 `int` can't read `"-"`: give it decimal digits, after `-` or `+` or neither
               1 `int` can't read `"0x"`: give it decimal digits, after `-` or `+` or neither
               1 `join` can't join an int: give it a list or a tuple of strings
               1 `join` can't take `None`: give it a list or a tuple of strings
               1 `join` can't take a string: give it a list or a tuple of strings
               1 `join` is not defined: bind it in this file, or load it
               1 `len` can't take an int: give it a list, a tuple or a dict
               1 `mm` is given by position already: give it once
               1 `popitem`, which would change the dict, is not in Pumpkin's Starlark: make a new dict with `|`, as values never change
               1 `print`, a builtin of Starlark's whose output nothing reads, is not in Pumpkin's Starlark: show a value with `fail`
               1 `replace` with 3 arguments is not in Pumpkin's Starlark: give it 2 arguments
               1 `return` stands only in a function's body: move it into a `def`
               1 `rindex` of a string is not in Pumpkin's Starlark: call one of `endswith`, `format`, `join`, `replace`, `split`, `startswith`
               1 `rsplit` of a string is not in Pumpkin's Starlark: call one of `endswith`, `format`, `join`, `replace`, `split`, `startswith`
               1 `sorted` can't take a string: give it a list, a tuple or a dict
               1 `sorted` can't take an int: give it a list, a tuple or a dict
               1 `sorted`'s `reverse` is not in Pumpkin's Starlark: give `sorted` 1 argument, by position
               1 `starts_with` is no method of a string: call one of `endswith`, `format`, `join`, `replace`, `split`, `startswith`
               1 `startswith`'s argument can't be `1`: give a string
               1 `str` of a function is not in Pumpkin's Starlark: give it an int or a string
               1 `str` of a list is not in Pumpkin's Starlark: give it an int or a string
               1 `update`, which would change the dict, is not in Pumpkin's Starlark: make a new dict with `|`, as values never change
               1 `z` is not defined: bind it in this file, or load it
               1 `{+0}` is no field of a `format` string: write `{}`, `{0}` or `{name}`
               1 `{+1}` is no field of a `format` string: write `{}`, `{0}` or `{name}`
               1 `{-0}`, a field that some Starlark implementations read as `{0}`, is not in Pumpkin's Starlark: write `{0}`
               1 `{-1}` is no field of a `format` string: write `{}`, `{0}` or `{name}`
               1 `{123}` names no argument of this `format` call, which has 0 by position: give it one more
               1 `{a.b}` is no field of a `format` string: write `{}`, `{0}` or `{name}`
               1 `{a[0]}` is no field of a `format` string: write `{}`, `{0}` or `{name}`
               1 `{z}` names no keyword argument of this `format` call: give it `z = …`
               1 a bool as a dict's key is not in Pumpkin's Starlark: key a dict with strings or ints
               1 a list of 0 values can't be unpacked into 2 names: give a list or a tuple of 2
               1 a list of 1 value can't be unpacked into 0 names: give a list or a tuple of 0
               1 a list of 1 value can't be unpacked into 2 names: give a list or a tuple of 2
               1 a list of 2 values can't be unpacked into 0 names: give a list or a tuple of 0
               1 a list of 2 values can't be unpacked into 3 names: give a list or a tuple of 3
               1 a list of 3 values can't be unpacked into 1 name: give a list or a tuple of 1
               1 a method named but not called is not in Pumpkin's Starlark: call it here, as `.index(…)`
               1 a method named but not called is not in Pumpkin's Starlark: call it here, as `.pop(…)`
               1 a method named but not called is not in Pumpkin's Starlark: call it here, as `.popitem(…)`
               1 a method named but not called is not in Pumpkin's Starlark: call it here, as `.upper(…)`
               1 a set literal is not in Starlark: write a list, or give each key a value after `:`
               1 a string can't be iterated over: split it into a list
               1 a string can't index a list: index it with an int
               1 a tuple has no method `append`: give it to a function, as `len`
               1 a tuple has no method `extend`: give it to a function, as `len`
               1 a tuple has no method `insert`: give it to a function, as `len`
               1 a tuple has no method `pop`: give it to a function, as `len`
               1 a tuple has no method `remove`: give it to a function, as `len`
               1 a tuple of 2 values can't be unpacked into 0 names: give a list or a tuple of 0
               1 an `if` statement stands only in a function's body: choose a value with `a if c else b`, or move the statement into a `def`
               1 an int can't be assigned to: assign to a name, or to names in a tuple
               1 this `format` string has a `{` that opens no field: close it with `}`, or write `{{` for a `{`
               1 this conditional expression has no `else`: give the value when the condition is false, after `else`
               1 this line is indented as no line above it is: indent it as far as a line of the block it is in
               1 this string is not closed on its line: end it with `'`, or quote it with `'''` to write several lines
               1 unexpected `)`: expected the end of the line
               1 unexpected `*`: expected an expression
               1 unexpected `,`: expected `]`
               1 unexpected `-`: expected `,` or `)`
               1 unexpected `1`: expected the end of the line
               1 unexpected `1`: expected the label of the file to load, as `"//lib/x.star"`
               1 unexpected `:`: expected `(`
               1 unexpected `=`: expected `(`
               1 unexpected `=`: expected `,` or `)`
               1 unexpected `conformance_done`: expected `,` or `)`
               1 unexpected `load`: expected a parameter's name, or `*`
               1 unexpected `load`: expected an expression
               1 unexpected `load`: expected the function's name
               1 unexpected `not`: expected the end of the line
               1 unexpected `pass`: expected the function's name"#]],
    );
}
