//! The ledger of the tests that `cargo xtask ci` ran, and the flakes in it.
//!
//! The ledger is a file of JSON lines, one for each run of `ci`'s tests. It is a history across
//! runs, as `ci` has no retries that would mark a flaky test within one run. A test is flaky on a
//! tree if some run failed it and another passed it on that tree. Google defines a flaky result
//! alike: [a test that exhibits both a passing and a failing result with the same
//! code](https://testing.googleblog.com/2016/05/flaky-tests-at-google-and-how-we.html).
//! Mozilla's sheriffs record each [intermittent
//! failure](https://firefox-source-docs.mozilla.org/testing/sheriffed-intermittents/index.html)
//! with its log. Each run here records its failures with their first lines of output.
//!
//! nextest writes each run's results as a [JUnit
//! report](https://github.com/nextest-rs/nextest/blob/6b3f994577a98874a2e0ea5bef77fbbab3434044/site/src/docs/machine-readable/junit.md#L12-L22),
//! which `.config/nextest.toml` enables.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::Path;

use serde_json::{Value, json};
use stdx::Context as _;

use crate::Result;
use crate::repo::Repo;

/// The ledger's file name in the state directory.
pub(crate) const LEDGER: &str = "ci-tests.jsonl";

/// How many characters of a commit or a tree the report shows.
const ID_CHARS: usize = 10;

/// The most lines of a failure's output that the ledger keeps.
const OUTPUT_LINES_MAX: usize = 10;

/// The most bytes of a failure's output that the ledger keeps, which a few long lines may reach.
const OUTPUT_BYTES_MAX: usize = 2048;

/// The largest ledger that [`report`] reads. About a thousand failing runs fill a megabyte, so
/// this holds the history of years of `ci` runs.
const LEDGER_BYTES_MAX: u64 = 64 << 20;

/// The commit and the working tree that a run tested.
#[derive(PartialEq, Eq)]
pub(crate) struct Tested {
    head: String,
    tree: String,
    /// Whether the working tree differs from `head`'s.
    changed: bool,
}

impl Tested {
    /// What `repo`'s working tree is now, as [`Repo::working_tree`] identifies it. Git ignores some
    /// files, and `ci` doesn't read them but through what it builds: the tools' links in `bin`
    /// and `target`.
    pub(crate) fn of(repo: &Repo<'_>) -> Result<Self> {
        let tree = repo.working_tree()?;
        Ok(Self {
            head: repo.git(["rev-parse", "HEAD"])?,
            changed: tree != repo.git(["rev-parse", "HEAD^{tree}"])?,
            tree,
        })
    }
}

/// A test that failed, with the first lines of its output.
struct Failure {
    test: String,
    output: String,
}

/// Appends to `ledger` a run of `ci`'s tests against `tested`, which nextest reported in `junit`.
/// `passed` is whether all of `ci`'s tests passed.
pub(crate) fn record(ledger: &Path, tested: &Tested, junit: &str, passed: bool) -> Result<()> {
    let (at, failures) = parse(junit)?;
    let Tested {
        head,
        tree,
        changed,
    } = tested;
    let failed: Vec<Value> = (failures.iter())
        .map(|Failure { test, output }| json!({ "test": test, "output": output }))
        .collect();
    let run = json!({
        "at": at,
        "head": head,
        "tree": tree,
        "changed": changed,
        "passed": passed,
        "failed": failed,
    });
    append(ledger, &format!("{run}\n"))
}

/// Appends `line`, whole, to `ledger`, which it makes if need be.
pub(crate) fn append(ledger: &Path, line: &str) -> Result<()> {
    if let Some(dir) = ledger.parent() {
        fs::create_dir_all(dir).context(format_args!("making {} failed", dir.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(ledger)
        .context(format_args!("opening {} failed", ledger.display()))?;
    // Two runs of `ci` may end together, in checkouts that share the ledger. The second waits for
    // the first, as a write of this size to a file may reach it in parts.
    file.lock()
        .context(format_args!("locking {} failed", ledger.display()))?;
    file.write_all(line.as_bytes())
        .context(format_args!("writing {} failed", ledger.display()))?;
    Ok(())
}

/// The time that the report `junit` gives for its run, and the tests that failed in it.
fn parse(junit: &str) -> Result<(String, Vec<Failure>)> {
    let Some(suites) = element(junit, "testsuites") else {
        return Err("nextest's report has no `testsuites` element: expected a JUnit report".into());
    };
    let Some(at) = attribute(suites.attributes, "timestamp") else {
        return Err("nextest's report gives no time for its run: expected a `timestamp`".into());
    };
    let mut failures = Vec::new();
    let mut rest = suites.inner;
    while let Some(case) = element(rest, "testcase") {
        rest = case.rest;
        // A failed test has a `failure` child, and one that nextest couldn't run, an `error`.
        let Some(failure) = element(case.inner, "failure").or_else(|| element(case.inner, "error"))
        else {
            continue;
        };
        let (Some(class), Some(name)) = (
            attribute(case.attributes, "classname"),
            attribute(case.attributes, "name"),
        ) else {
            return Err(
                "nextest's report has a test without a name: expected a JUnit report".into(),
            );
        };
        let text = unescape(failure.inner);
        // A test that nextest killed at its timeout has no text, only its kind.
        let text = match text.trim() {
            "" => attribute(failure.attributes, "type").unwrap_or_default(),
            text => text.to_owned(),
        };
        failures.push(Failure {
            // As nextest prints a test.
            test: format!("{class} {name}"),
            output: first_lines(&text),
        });
    }
    Ok((at, failures))
}

fn first_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().take(OUTPUT_LINES_MAX).collect();
    let mut output = lines.join("\n");
    let mut end = output.len().min(OUTPUT_BYTES_MAX);
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    output.truncate(end);
    output
}

/// An XML element, as nextest writes it: its tag's attributes, its text up to its end tag, and what
/// follows it.
struct Element<'a> {
    attributes: &'a str,
    inner: &'a str,
    rest: &'a str,
}

/// The first element called `name` in `xml`, in the shape that nextest writes. An element has no
/// element of its own name inside it, and a `>` in an attribute is escaped.
fn element<'a>(xml: &'a str, name: &str) -> Option<Element<'a>> {
    let open = format!("<{name}");
    let mut from = 0;
    let after_name = loop {
        let at = from + xml[from..].find(&open)?;
        let after = at + open.len();
        // Not an element called `name` followed by more letters, such as `testsuites`.
        if xml[after..].starts_with([' ', '>', '/']) {
            break after;
        }
        from = after;
    };
    let tag_end = after_name + xml[after_name..].find('>')?;
    let attributes = &xml[after_name..tag_end];
    let after_tag = &xml[tag_end + 1..];
    if let Some(attributes) = attributes.strip_suffix('/') {
        return Some(Element {
            attributes,
            inner: "",
            rest: after_tag,
        });
    }
    let close = format!("</{name}>");
    let inner_end = after_tag.find(&close)?;
    Some(Element {
        attributes,
        inner: &after_tag[..inner_end],
        rest: &after_tag[inner_end + close.len()..],
    })
}

/// The value of the attribute `name` in `attributes`, the text of a start tag after its name.
fn attribute(attributes: &str, name: &str) -> Option<String> {
    let open = format!(" {name}=\"");
    let from = attributes.find(&open)? + open.len();
    let value = &attributes[from..];
    Some(unescape(&value[..value.find('"')?]))
}

const ENTITIES: [(&str, char); 5] = [
    ("&lt;", '<'),
    ("&gt;", '>'),
    ("&amp;", '&'),
    ("&quot;", '"'),
    ("&apos;", '\''),
];

/// `text` with the five entities that nextest's XML writer escapes replaced.
fn unescape(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        plain.push_str(&rest[..at]);
        rest = &rest[at..];
        if let Some(&(entity, unescaped)) =
            ENTITIES.iter().find(|(entity, _)| rest.starts_with(entity))
        {
            plain.push(unescaped);
            rest = &rest[entity.len()..];
        } else {
            plain.push('&');
            rest = &rest[1..];
        }
    }
    plain.push_str(rest);
    plain
}

/// A run of `ci`'s tests, as the ledger holds it.
struct Run {
    at: String,
    head: String,
    tree: String,
    changed: bool,
    passed: bool,
    failures: Vec<Failure>,
}

impl Run {
    fn parse(line: &str) -> Option<Self> {
        let run: Value = serde_json::from_str(line).ok()?;
        let text = |value: &Value| value.as_str().map(str::to_owned);
        let failures = (run["failed"].as_array()?.iter())
            .map(|failure| {
                Some(Failure {
                    test: text(&failure["test"])?,
                    output: text(&failure["output"])?,
                })
            })
            .collect::<Option<_>>()?;
        Some(Self {
            at: text(&run["at"])?,
            head: text(&run["head"])?,
            tree: text(&run["tree"])?,
            changed: run["changed"].as_bool()?,
            passed: run["passed"].as_bool()?,
            failures,
        })
    }
}

/// What the ledger holds of one tree.
struct Tree<'a> {
    head: &'a str,
    changed: bool,
    passed_count: usize,
}

/// One failure of a test.
struct Failed<'a> {
    tree: &'a str,
    at: &'a str,
    output: &'a str,
}

/// The report of the tests in `ledger` that failed and passed on one tree.
pub(crate) fn report(ledger: &Path) -> Result<String> {
    let bytes = match fs::metadata(ledger) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(format!(
                "{} doesn't exist: `cargo xtask ci` makes it when its tests run",
                ledger.display()
            )
            .into());
        }
        Err(error) => {
            return Err(format!("reading {} failed: {error}", ledger.display()).into());
        }
    };
    if bytes > LEDGER_BYTES_MAX {
        return Err(format!(
            "{} holds {bytes} bytes, more than the {LEDGER_BYTES_MAX} that `flakes` reads: move it \
             aside to start a new ledger",
            ledger.display()
        )
        .into());
    }
    let text =
        fs::read_to_string(ledger).context(format_args!("reading {} failed", ledger.display()))?;
    Ok(flakes(&text))
}

fn flakes(ledger: &str) -> String {
    let parsed: Vec<Option<Run>> = ledger.lines().map(Run::parse).collect();
    let unread_count = parsed.iter().filter(|run| run.is_none()).count();
    let runs: Vec<Run> = parsed.into_iter().flatten().collect();
    let mut trees = BTreeMap::<&str, Tree>::new();
    let mut failed = BTreeMap::<&str, Vec<Failed<'_>>>::new();
    for run in &runs {
        let tree = trees.entry(&run.tree).or_insert_with(|| Tree {
            head: &run.head,
            changed: run.changed,
            passed_count: 0,
        });
        tree.passed_count += usize::from(run.passed);
        for failure in &run.failures {
            failed.entry(&failure.test).or_default().push(Failed {
                tree: &run.tree,
                at: &run.at,
                output: &failure.output,
            });
        }
    }
    let mut report = format!("ci runs: {}, trees: {}\n", runs.len(), trees.len());
    if unread_count > 0 {
        let _ = writeln!(report, "{unread_count} lines of the ledger are no run");
    }
    // A failure counts where its tree has a run that passed.
    let mut flaky: Vec<(&str, Vec<Failed<'_>>)> = Vec::new();
    for (test, failures) in failed {
        let failures: Vec<_> = (failures.into_iter())
            .filter(|failure| trees[failure.tree].passed_count > 0)
            .collect();
        if !failures.is_empty() {
            flaky.push((test, failures));
        }
    }
    // The tests that failed most come first.
    flaky.sort_by_key(|(test, failures)| (Reverse(failures.len()), *test));
    if flaky.is_empty() {
        report.push_str("no test failed and passed on one tree\n");
    }
    for (test, failures) in flaky {
        let ids: BTreeSet<&str> = failures.iter().map(|failure| failure.tree).collect();
        let passed_count: usize = ids.iter().map(|id| trees[id].passed_count).sum();
        let _ = writeln!(
            report,
            "\n{test}: failed {}, passed {passed_count}",
            failures.len()
        );
        for id in ids {
            let Tree { head, changed, .. } = &trees[id];
            let changes = if *changed {
                ", with uncommitted changes"
            } else {
                ""
            };
            let _ = writeln!(
                report,
                "  tree {} (head {}{changes})",
                short(id),
                short(head)
            );
        }
        if let Some(Failed { at, output, .. }) = failures.last() {
            let _ = writeln!(report, "  last failure, {at}:");
            for line in output.lines() {
                let _ = writeln!(report, "    {line}");
            }
        }
    }
    report
}

/// The first characters of a hash, as `git` abbreviates one.
fn short(hash: &str) -> &str {
    hash.get(..ID_CHARS).unwrap_or(hash)
}
