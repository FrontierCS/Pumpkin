use std::fmt::Write as _;
use std::path::Path;
use std::{fs, thread};

use expect_test::expect;
use test_support::TestDir;

use super::{Standing, check, write};
use crate::flakes::{self, Tested};
use crate::repo::Repo;

/// A `JUnit` report in the shape that nextest writes, with the tests that `failed`. Each has a name
/// and the text of its failure, or none if nextest killed it at its timeout.
fn junit(at: &str, failed: &[(&str, Option<&str>)]) -> String {
    let mut junit = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites name=\"nextest-run\" tests=\"9\" \
         timestamp=\"{at}\" time=\"1.0\">\n    <testsuite name=\"pkg::it\" tests=\"9\">\n        \
         <testcase name=\"fine\" classname=\"pkg::it\" time=\"0.010\"/>\n"
    );
    for (name, text) in failed {
        let _ = match text {
            Some(text) => {
                let text = text
                    .replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
                    .replace('\'', "&apos;");
                write!(
                    junit,
                    "        <testcase name=\"{name}\" classname=\"pkg::it\" time=\"0.011\">\n            \
                     <failure message=\"panicked\" type=\"test failure with exit code 101\">{text}</failure>\n            \
                     <system-out>\nrunning 1 test\n</system-out>\n        </testcase>\n"
                )
            }
            None => write!(
                junit,
                "        <testcase name=\"{name}\" classname=\"pkg::it\" time=\"5.0\">\n            \
                 <failure type=\"test timeout\"/>\n        </testcase>\n"
            ),
        };
    }
    junit.push_str("    </testsuite>\n</testsuites>\n");
    junit
}

/// Commits what the working tree holds, which `check` leaves its `.gitignore` out of.
fn commit_all(repo: &Repo<'_>) {
    repo.git(["add", "--all"]).unwrap();
    let author = [
        "-c",
        "user.name=xtask",
        "-c",
        "user.email=xtask@example.com",
    ];
    let commit = ["commit", "--quiet", "--message", "all"];
    repo.git(author.into_iter().chain(commit)).unwrap();
}

/// Records a run of tests against `repo`'s working tree as it is.
fn run(ledger: &Path, repo: &Repo<'_>, at: &str, failed: &[(&str, Option<&str>)], passed: bool) {
    let tested = Tested::of(repo).unwrap();
    flakes::record(ledger, &tested, &junit(at, failed), passed).unwrap();
}

#[test]
fn reports_failures_that_a_pass_on_the_same_tree_matches() {
    let panicked = "thread 'slow' panicked at src/a.rs:1:1:\nassertion `left == right` failed\n  left: <1>\n right: 2\n";
    let long = format!("{panicked}{}", "a line\n".repeat(12));
    let shorter = "thread 'slow' panicked at src/a.rs:9:9:\nit failed again\n";
    check(
        &[("a.rs", "fn a() {}\n".to_owned(), Standing::Tracked)],
        |repo| {
            commit_all(repo);
            // Inside the repository, so that the ledger is no change to it.
            let state = repo.root.join(".git/state");
            let ledger = state.join("pumpkin").join(flakes::LEDGER);
            let head = repo.git(["rev-parse", "HEAD"]).unwrap();
            let first = repo.working_tree().unwrap();
            // Fails, then passes, on the first tree.
            run(
                &ledger,
                repo,
                "t1",
                &[("slow", Some(shorter)), ("hangs", None)],
                false,
            );
            run(&ledger, repo, "t2", &[], true);
            // Fails, then passes, on a tree with uncommitted changes.
            write(repo.root, "a.rs", "fn a() { 1; }\n");
            let second = repo.working_tree().unwrap();
            run(&ledger, repo, "t3", &[("slow", Some(&long))], false);
            run(&ledger, repo, "t4", &[], true);
            run(&ledger, repo, "t5", &[("once", Some("it failed"))], false);
            // Fails, and never passes, on a third.
            write(repo.root, "a.rs", "fn a() { 2; }\n");
            run(&ledger, repo, "t6", &[("never", Some("it failed"))], false);
            flakes::report(&ledger)
                .unwrap()
                .replace(&first[..10], "<first tree>")
                .replace(&second[..10], "<second tree>")
                .replace(&head[..10], "<head>")
        },
        &expect![[r#"
            ci runs: 6, trees: 3

            pkg::it slow: failed 2, passed 2
              tree <first tree> (head <head>)
              tree <second tree> (head <head>, with uncommitted changes)
              last failure, t3:
                thread 'slow' panicked at src/a.rs:1:1:
                assertion `left == right` failed
                  left: <1>
                 right: 2
                a line
                a line
                a line
                a line
                a line
                a line

            pkg::it hangs: failed 1, passed 1
              tree <first tree> (head <head>)
              last failure, t1:
                test timeout

            pkg::it once: failed 1, passed 1
              tree <second tree> (head <head>, with uncommitted changes)
              last failure, t5:
                it failed
        "#]],
    );
}

#[test]
fn identifies_the_working_tree_by_its_files() {
    let files = [
        ("a.rs", "fn a() {}\n".to_owned(), Standing::Tracked),
        ("b.rs", "fn b() {}\n".to_owned(), Standing::Tracked),
    ];
    check(
        &files,
        |repo| {
            commit_all(repo);
            let committed = repo.git(["rev-parse", "HEAD^{tree}"]).unwrap();
            let mut seen = Vec::new();
            let mut report = String::new();
            let mut step = |what: &str| {
                let tree = repo.working_tree().unwrap();
                let name = if tree == committed {
                    "the committed tree".to_owned()
                } else {
                    if !seen.contains(&tree) {
                        seen.push(tree.clone());
                    }
                    let index = seen.iter().position(|seen| *seen == tree).unwrap();
                    format!("tree {}", index + 1)
                };
                let _ = writeln!(report, "{what}: {name}");
            };
            step("as committed");
            write(repo.root, "untracked.rs", "fn u() {}\n");
            step("an untracked file");
            write(repo.root, "ignored/x.rs", "fn x() {}\n");
            step("and an ignored file");
            write(repo.root, "a.rs", "fn a() { 1; }\n");
            step("and a changed file");
            fs::remove_file(repo.root.join("untracked.rs")).unwrap();
            step("without the untracked file");
            write(repo.root, "a.rs", "fn a() {}\n");
            step("with the file as committed");
            fs::remove_file(repo.root.join("b.rs")).unwrap();
            step("a deleted file");
            write(repo.root, "b.rs", "fn b() {}\n");
            write(repo.root, "a.rs", "fn a() { 1; }\n");
            step("a changed file again");
            // Staging a change, which edits the index, leaves the working tree as it was.
            repo.git(["add", "--", "a.rs"]).unwrap();
            let staged = repo.git(["status", "--porcelain"]).unwrap();
            step("and staged");
            assert_eq!(repo.git(["status", "--porcelain"]).unwrap(), staged);
            let scratch: Vec<_> = fs::read_dir(repo.root.join(".git"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .filter(|name| name.starts_with("xtask-tree"))
                .collect();
            assert_eq!(scratch, [] as [String; 0]);
            report
        },
        &expect![[r#"
            as committed: the committed tree
            an untracked file: tree 1
            and an ignored file: tree 1
            and a changed file: tree 2
            without the untracked file: tree 3
            with the file as committed: the committed tree
            a deleted file: tree 4
            a changed file again: tree 3
            and staged: tree 3
        "#]],
    );
}

#[test]
fn appends_each_line_whole() {
    let dir = TestDir::new();
    let ledger = dir.path().join("ledger");
    // Longer than the 64 KiB that a pipe or a socket carries in one write.
    let lines: Vec<String> = (0..4)
        .map(|writer| {
            format!(
                "{}\n",
                char::from(b'a' + writer).to_string().repeat(1 << 18)
            )
        })
        .collect();
    thread::scope(|scope| {
        for line in &lines {
            scope.spawn(|| {
                for _ in 0..4 {
                    flakes::append(&ledger, line).unwrap();
                }
            });
        }
    });
    let written = fs::read_to_string(&ledger).unwrap();
    let mut found: Vec<&str> = written.lines().collect();
    found.sort_unstable();
    let mut expected: Vec<&str> = lines.iter().flat_map(|line| [line.trim_end(); 4]).collect();
    expected.sort_unstable();
    assert!(
        found == expected,
        "the lines of the ledger were cut or mixed"
    );
}
