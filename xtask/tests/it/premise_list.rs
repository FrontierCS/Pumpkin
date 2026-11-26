use std::fs;

use expect_test::expect;

use super::{Standing, check, write};
use crate::premises;
use crate::repo::Repo;

const BEFORE: &str = r#"fn kept() {
    unreachable!("kept");
}

fn edited() {
    let a = x.unwrap();
}
"#;

const AFTER: &str = r#"fn kept() {
    unreachable!("kept");
}

fn edited() {
    let a = x.unwrap();
    let b = y.expect("the map holds the key");
    assert!(a < 3, "a is {a}");
    debug_assert!(b);
    assert_eq!(a, b);
    #[expect(clippy::panic, reason = "it is the one place")]
    panic!(
        "{}  spread over
         lines",
        a
    );
    let c = text.parse::<u8>().unwrap_or(0);
    let d = ".unwrap() in a string";
    // x.unwrap() in a comment
    nested(opt.unwrap().expect("inner"));
    match n {
        0 => unreachable!(),
        _ => {}
    }
}

#[test]
fn a_test() {
    z.unwrap();
}

#[cfg(test)]
mod tests {
    fn helper() {
        z.unwrap();
    }
}

#[cfg(not(test))]
fn built() {
    w.unwrap();
}
"#;

/// Lists the premises that the working tree adds or changes since the commit that `check` made.
fn list(repo: &Repo<'_>) -> String {
    premises::list(repo, "HEAD").unwrap()
}

#[test]
fn lists_what_a_change_adds() {
    check(
        &[
            ("crates/a/src/lib.rs", BEFORE.to_owned(), Standing::Tracked),
            (
                "crates/a/tests/it.rs",
                "fn t() {}\n".to_owned(),
                Standing::Tracked,
            ),
            (
                "crates/test-support/src/lib.rs",
                "fn s() {}\n".to_owned(),
                Standing::Tracked,
            ),
            ("crates/a/notes.md", "text\n".to_owned(), Standing::Tracked),
        ],
        |repo| {
            write(repo.root, "crates/a/src/lib.rs", AFTER);
            write(
                repo.root,
                "crates/a/tests/it.rs",
                "fn t() { x.unwrap(); }\n",
            );
            write(
                repo.root,
                "crates/test-support/src/lib.rs",
                "fn s() { x.unwrap(); }\n",
            );
            write(repo.root, "crates/a/notes.md", "x.unwrap()\n");
            list(repo)
        },
        &expect![[r#"
            crates/a/src/lib.rs:7: .expect("the map holds the key")
            crates/a/src/lib.rs:8: assert!(a < 3, "a is {a}")
            crates/a/src/lib.rs:9: debug_assert!(b)
            crates/a/src/lib.rs:12: panic!("{} spread over lines", a)
            crates/a/src/lib.rs:20: .unwrap() in `nested(opt.unwrap().expect("inner"));`
            crates/a/src/lib.rs:20: .expect("inner")
            crates/a/src/lib.rs:22: unreachable!() in `0 => unreachable!(),`
            crates/a/src/lib.rs:41: .unwrap() in `w.unwrap();`
            premises: 8, files: 1
        "#]],
    );
}

#[test]
fn leaves_out_what_the_change_doesnt_touch() {
    check(
        &[
            ("kept.rs", BEFORE.to_owned(), Standing::Tracked),
            (
                "moved.rs",
                "fn m() { a.unwrap(); }\n".to_owned(),
                Standing::Tracked,
            ),
            (
                "gone.rs",
                "fn g() { a.unwrap(); }\n".to_owned(),
                Standing::Tracked,
            ),
            ("broken.rs", "fn b() {}\n".to_owned(), Standing::Tracked),
        ],
        |repo| {
            repo.git(["mv", "moved.rs", "renamed.rs"]).unwrap();
            fs::remove_file(repo.root.join("gone.rs")).unwrap();
            write(repo.root, "broken.rs", "fn b() { \"open\n");
            // An untracked file is in the diff once it is added with intent.
            write(repo.root, "new.rs", "fn n() { v.unwrap(); }\n");
            repo.git(["add", "--intent-to-add", "--", "new.rs"])
                .unwrap();
            list(repo)
        },
        &expect![[r#"
            new.rs:1: .unwrap() in `fn n() { v.unwrap(); }`
            broken.rs: not read: cannot parse string into token stream
            premises: 1, files: 1
        "#]],
    );
}
