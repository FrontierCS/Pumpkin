//! xtask's checks of Rust files ([`tidy`]) and its diff ([`diff`]), on scratch Git
//! repositories, and the consumer entry `deploy/pumpkin` ([`entry`]). xtask is a binary, so its
//! test binary compiles their modules itself.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::error::Error;

use expect_test::{Expect, expect};
use xshell::{Shell, cmd};

#[path = "../../src/diff.rs"]
mod diff;
mod entry;
#[path = "../../src/tidy.rs"]
mod tidy;

/// The result the check returns, as xtask's tasks do.
type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// How a file stands in the scratch repository.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Standing {
    /// In the repository's one commit.
    Tracked,
    Untracked,
    /// In a directory the repository ignores.
    Ignored,
    /// Tracked, then deleted from the working tree.
    Deleted,
}

/// Writes `files`, each a path, its text and how it stands, into a new Git repository, and runs
/// `run` there; the repository's path shows as `<repo>`.
fn check(
    name: &str,
    files: &[(&str, String, Standing)],
    run: impl FnOnce(&Shell) -> String,
    expect: &Expect,
) {
    let scratch = tempfile::Builder::new()
        .prefix(&format!("xtask-{name}-"))
        .tempdir()
        .unwrap();
    let dir = scratch.path();
    let sh = Shell::new().unwrap();
    sh.change_dir(dir);
    // Git's own variables, inherited, would steer every command here to another repository, as
    // the `GIT_DIR` that `git rebase --exec` sets in a linked worktree would. A `Shell` can set
    // a variable but not remove one, so each that places a repository is set to this one's.
    let git = dir.join(".git");
    sh.set_var("GIT_DIR", &git);
    sh.set_var("GIT_COMMON_DIR", &git);
    sh.set_var("GIT_WORK_TREE", dir);
    sh.set_var("GIT_INDEX_FILE", git.join("index"));
    sh.set_var("GIT_OBJECT_DIRECTORY", git.join("objects"));
    // Nor does the user's configuration, which could change what a diff prints.
    sh.set_var("GIT_CONFIG_GLOBAL", "/dev/null");
    sh.set_var("GIT_CONFIG_NOSYSTEM", "1");
    for name in ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"] {
        sh.set_var(name, "xtask");
    }
    for email in ["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"] {
        sh.set_var(email, "xtask@example.com");
    }
    cmd!(sh, "git init --quiet").quiet().run().unwrap();
    sh.write_file(".gitignore", "ignored/\n").unwrap();
    for (path, text, standing) in files {
        sh.write_file(path, text).unwrap();
        if matches!(standing, Standing::Tracked | Standing::Deleted) {
            cmd!(sh, "git add -- {path}").quiet().run().unwrap();
        }
        if *standing == Standing::Deleted {
            sh.remove_path(path).unwrap();
        }
    }
    cmd!(sh, "git commit --quiet --allow-empty --message base")
        .quiet()
        .run()
        .unwrap();
    let reported = run(&sh);
    expect.assert_eq(&reported.replace(dir.to_str().unwrap(), "<repo>"));
}

/// What the checks of Rust files report.
fn tidy(sh: &Shell) -> String {
    match tidy::check(sh) {
        Ok(()) => "passed".to_owned(),
        Err(error) => error.to_string(),
    }
}

/// `count` lines, each `line`.
fn lines(line: &str, count: usize) -> String {
    format!("{line}\n").repeat(count)
}

#[test]
fn caps() {
    use Standing::{Deleted, Ignored, Tracked, Untracked};
    let at_caps = lines("//! A line.", 25) + &lines("/// A line.", 9) + "fn f() {}\n";
    let behind_attribute = format!(
        "#![expect(\n    dead_code,\n    reason = \"a reason\"\n)]\n{}",
        lines("//! A line.", 26)
    );
    let in_parts = format!(
        "/* A comment. */\n{}\n#![doc = \"A line.\"]\n{}",
        lines("//! A line.", 15),
        lines("//! A line.", 10)
    );
    let item_in_parts = format!(
        "{}#[derive(Debug)]\n\n// A comment.\n{}#[doc = \"A line.\"]\nstruct S;\n",
        lines("/// A line.", 5),
        lines("/// A line.", 4)
    );
    let raw = format!(
        "const S: &str = r#\"\n{}\"#;\nconst B: &[u8] = br\"\n{}\";\nfn f() {{}}\n",
        lines("/// Not a doc.", 20),
        lines("//! Not a doc.", 41)
    );
    let separate =
        lines("/// A line.", 9) + "fn a() {}\n" + &lines("/// A line.", 9) + "fn b() {}\n";
    let long_item = lines("/// A line.", 10) + "fn f() {}\n";
    let method = format!(
        "struct S;\nimpl S {{\n{}    fn f() {{}}\n}}\n",
        lines("    /// A line.", 10)
    );
    let inline_module = format!("mod m {{\n{}}}\n", lines("    //! A line.", 26));
    let macro_rule = format!(
        "macro_rules! m {{\n    () => {{\n{}        fn f() {{}}\n    }};\n}}\n",
        lines("        /// A line.", 10)
    );
    check(
        "caps",
        &[
            ("at_caps.rs", at_caps, Tracked),
            ("behind_attribute.rs", behind_attribute, Tracked),
            ("in_parts.rs", in_parts, Tracked),
            ("item_in_parts.rs", item_in_parts, Tracked),
            ("raw.rs", raw, Tracked),
            ("separate.rs", separate, Tracked),
            ("method.rs", method, Tracked),
            ("inline_module.rs", inline_module, Tracked),
            ("macro_rule.rs", macro_rule, Tracked),
            ("é.rs", long_item.clone(), Tracked),
            ("untracked.rs", long_item.clone(), Untracked),
            ("ignored/long.rs", long_item.clone(), Ignored),
            ("deleted.rs", long_item, Deleted),
        ],
        tidy,
        &expect![[r#"
            untracked.rs:1: an item's doc is 10 lines, over the cap of 9: shorten it to 9 or fewer, moving each fact to its home
            behind_attribute.rs:5: the module doc is 26 lines, over the cap of 25: shorten it to 25 or fewer, moving each fact to its home
            in_parts.rs:2: the module doc is 26 lines, over the cap of 25: shorten it to 25 or fewer, moving each fact to its home
            inline_module.rs:2: the module doc is 26 lines, over the cap of 25: shorten it to 25 or fewer, moving each fact to its home
            item_in_parts.rs:1: an item's doc is 10 lines, over the cap of 9: shorten it to 9 or fewer, moving each fact to its home
            macro_rule.rs:3: an item's doc is 10 lines, over the cap of 9: shorten it to 9 or fewer, moving each fact to its home
            method.rs:3: an item's doc is 10 lines, over the cap of 9: shorten it to 9 or fewer, moving each fact to its home
            é.rs:1: an item's doc is 10 lines, over the cap of 9: shorten it to 9 or fewer, moving each fact to its home"#]],
    );
}

#[test]
fn forms() {
    use Standing::Tracked;
    let example = format!(
        "/// ```\n/// let s = r#\"\n{}/// \"#;\n/// ```\nfn f() {{}}\n",
        lines("/// A line.", 6)
    );
    let strings = format!(
        "const A: &str = \"-r\";\nconst B: &str = \"r#\";\nconst C: &CStr = cr\"\n{}\";\n\
         const D: &str = \"\n{}\";\n{}fn f() {{}}\n",
        lines("/// Not a doc.", 16),
        lines("//! Not a doc.", 41),
        lines("/// A line.", 10)
    );
    let bracket = format!(
        "#[expect(\n    dead_code,\n    reason = \"a [bracket\"\n)]\nfn a() {{}}\n{}fn b() {{}}\n",
        lines("/// A line.", 10)
    );
    let blocks = format!(
        "/*!\n{}*/\n/**\n{}*/\nfn f() {{}}\n",
        lines(" A line.", 24),
        lines(" A line.", 8)
    );
    let attributes = format!(
        "#[doc = \"A line.\n{}\"]\nfn f() {{}}\n{}#[doc(hidden)]\nfn g() {{}}\n",
        lines("A line.", 8),
        lines("/// A line.", 9)
    );
    let invalid = "fn f() {\n    let c = 'ab';\n}\n".to_owned();
    check(
        "forms",
        &[
            ("example.rs", example, Tracked),
            ("strings.rs", strings, Tracked),
            ("bracket.rs", bracket, Tracked),
            ("blocks.rs", blocks, Tracked),
            ("attributes.rs", attributes, Tracked),
            ("invalid.rs", invalid, Tracked),
        ],
        tidy,
        &expect![[r#"
            attributes.rs:1: an item's doc is 10 lines, over the cap of 9: shorten it to 9 or fewer, moving each fact to its home
            blocks.rs:1: the module doc is 26 lines, over the cap of 25: shorten it to 25 or fewer, moving each fact to its home
            blocks.rs:27: an item's doc is 10 lines, over the cap of 9: shorten it to 9 or fewer, moving each fact to its home
            bracket.rs:6: an item's doc is 10 lines, over the cap of 9: shorten it to 9 or fewer, moving each fact to its home
            example.rs:1: an item's doc is 10 lines, over the cap of 9: shorten it to 9 or fewer, moving each fact to its home
            invalid.rs:2:13: the file is not valid Rust here, so it can't be checked: make it valid Rust
            strings.rs:64: an item's doc is 10 lines, over the cap of 9: shorten it to 9 or fewer, moving each fact to its home"#]],
    );
}

#[test]
fn unreachable() {
    use Standing::Tracked;
    let bare = format!(
        "fn f() {{\n    unreachable!()\n}}\n{}fn g() {{\n    std::unreachable![];\n}}\n\
         macro_rules! m {{\n    () => {{ core::unreachable! {{}} }};\n}}\n\
         fn h(x: u8) {{\n    debug_assert!(x > 0);\n    std::debug_assert_eq!(x, 1, \"why\");\n    \
         debug_assert_ne![x, 2];\n}}\n",
        lines("/// A line.", 10)
    );
    let messages = "#![expect(unreachable_pub, reason = \"unreachable!()\")]\n\
                    fn f(x: u8) {\n    unreachable!(\"x is {x}\");\n    unreachable!(\"why\", );\n\
                    // unreachable!()\n    let _ = \"unreachable!()\";\n    unreachable(());\n}\n\
                    fn h(x: u8) {\n    assert!(x > 0);\n    assert_eq!(x, 1);\n    \
                    // debug_assert!(x)\n    let _ = \"debug_assert!(x)\";\n    debug_assert(x);\n}\n"
        .to_owned();
    check(
        "unreachable",
        &[
            ("bare.rs", bare, Tracked),
            ("messages.rs", messages, Tracked),
        ],
        tidy,
        &expect![[r#"
            bare.rs:2:5: `unreachable!` has no message: say in one why only a bug can reach it
            bare.rs:4: an item's doc is 10 lines, over the cap of 9: shorten it to 9 or fewer, moving each fact to its home
            bare.rs:15:10: `unreachable!` has no message: say in one why only a bug can reach it
            bare.rs:18:19: `unreachable!` has no message: say in one why only a bug can reach it
            bare.rs:21:5: `debug_assert!` is skipped by release builds: use `assert!`, which guards the real run too
            bare.rs:22:10: `debug_assert_eq!` is skipped by release builds: use `assert_eq!`, which guards the real run too
            bare.rs:23:5: `debug_assert_ne!` is skipped by release builds: use `assert_ne!`, which guards the real run too"#]],
    );
}

#[test]
fn test_docs() {
    use Standing::Tracked;
    let tests = "/// A doc.\n#[test]\nfn before() {}\n\
                 #[test]\n/// A doc.\n#[ignore = \"a reason\"]\nfn between() {}\n\
                 mod m {\n    #[test]\n    /** A doc. */\n    fn inner() {}\n}\n\
                 #[doc = \"A doc.\"]\n#[test]\nfn attribute() {}\n\
                 #[test]\n// A comment.\nfn commented() {}\n\
                 /// A doc.\nfn helper() {}\n\
                 /// A doc.\n#[cfg(test)]\nfn configured() {}\n"
        .to_owned();
    check(
        "test-docs",
        &[("tests.rs", tests, Tracked)],
        tidy,
        &expect![[r#"
        tests.rs:1:1: a test has a doc: remove it, and put a why the test needs in a `//` comment at the line it explains
        tests.rs:5:1: a test has a doc: remove it, and put a why the test needs in a `//` comment at the line it explains
        tests.rs:10:5: a test has a doc: remove it, and put a why the test needs in a `//` comment at the line it explains
        tests.rs:13:1: a test has a doc: remove it, and put a why the test needs in a `//` comment at the line it explains"#]],
    );
}

#[test]
fn comments() {
    use Standing::Tracked;
    let markers = "//! A TODO.\n/// A FIXME, then an XXX.\nfn f() {\n    // TODO: one.\n    \
                   let _ = 1; /* a FIXME */\n}\n/** XXX */\n/* Nested /* TODO */ */\nfn g() {}\n\
                   // TODOS, XXXX, FIXME_ONE, A_TODO and todo are words of their own.\n"
        .to_owned();
    let code = "fn f(v: &[u8]) -> Option<u8> {\n    // f(v);\n    // let x = 1;\n    \
                // if v.is_empty() {\n    // }\n    // v[0];\n    //// f(v)?;\n    \
                // A clause;\n    // a `v[0]` and its end;\n    Some(v[0])\n}\n\
                // At the end, with no newline: f(v);"
        .to_owned();
    let literals = "/// ```\n/// f(v);\n/// ```\n#[doc = \"A TODO.\"]\n\
                    fn f() {\n    /* f(v); */\n    let _ = \"// TODO: f(v);\";\n    \
                    let _ = r#\"\n// let x = 1;\n\"#;\n    let _ = '/';\n    let _ = 1 / 2;\n}\n"
        .to_owned();
    check(
        "comments",
        &[
            ("markers.rs", markers, Tracked),
            ("code.rs", code, Tracked),
            ("literals.rs", literals, Tracked),
        ],
        tidy,
        &expect![[r#"
            code.rs:2:5: a `//` comment holds code: remove it, as Git keeps what was there
            code.rs:3:5: a `//` comment holds code: remove it, as Git keeps what was there
            code.rs:4:5: a `//` comment holds code: remove it, as Git keeps what was there
            code.rs:5:5: a `//` comment holds code: remove it, as Git keeps what was there
            code.rs:6:5: a `//` comment holds code: remove it, as Git keeps what was there
            code.rs:7:5: a `//` comment holds code: remove it, as Git keeps what was there
            code.rs:12:1: a `//` comment holds code: remove it, as Git keeps what was there
            markers.rs:1:7: a comment holds `TODO`: open an issue for the work left, and say a known limit as a why at its code
            markers.rs:2:7: a comment holds `FIXME`: open an issue for the work left, and say a known limit as a why at its code
            markers.rs:2:22: a comment holds `XXX`: open an issue for the work left, and say a known limit as a why at its code
            markers.rs:4:8: a comment holds `TODO`: open an issue for the work left, and say a known limit as a why at its code
            markers.rs:5:21: a comment holds `FIXME`: open an issue for the work left, and say a known limit as a why at its code
            markers.rs:7:5: a comment holds `XXX`: open an issue for the work left, and say a known limit as a why at its code
            markers.rs:8:14: a comment holds `TODO`: open an issue for the work left, and say a known limit as a why at its code"#]],
    );
}

#[test]
fn diff() {
    use Standing::{Deleted, Ignored, Tracked, Untracked};
    let again = "cargo xtask mutants --in-diff HEAD";
    check(
        "diff",
        &[
            ("changed.rs", "fn f() {}\n".to_owned(), Tracked),
            ("deleted.rs", "fn d() {}\n".to_owned(), Deleted),
            ("new.rs", "fn g() {}\n".to_owned(), Untracked),
            ("notes.txt", "Notes.\n".to_owned(), Untracked),
            ("ignored/new.rs", "fn i() {}\n".to_owned(), Ignored),
        ],
        |sh| {
            sh.write_file("changed.rs", "fn f() {\n    g();\n}\n")
                .unwrap();
            let refused = diff::since(sh, "HEAD", &[], again).unwrap_err();
            cmd!(sh, "git add --intent-to-add -- new.rs")
                .quiet()
                .run()
                .unwrap();
            let diff = diff::since(sh, "HEAD", &[], again).unwrap();
            let below = diff::since(sh, "HEAD", &["changed.rs".into()], again).unwrap();
            format!("{refused}\n{diff}below changed.rs:\n{below}")
        },
        &expect![[r#"
            the diff would leave out untracked Rust files: add them to it with `git add --intent-to-add -- <repo>/new.rs`, then run `cargo xtask mutants --in-diff HEAD` again
            diff --git a/changed.rs b/changed.rs
            index 59760b5..e20511e 100644
            --- a/changed.rs
            +++ b/changed.rs
            @@ -1 +1,3 @@
            -fn f() {}
            +fn f() {
            +    g();
            +}
            diff --git a/deleted.rs b/deleted.rs
            deleted file mode 100644
            index d68b814..0000000
            --- a/deleted.rs
            +++ /dev/null
            @@ -1 +0,0 @@
            -fn d() {}
            diff --git a/new.rs b/new.rs
            new file mode 100644
            index 0000000..ed2b5d2
            --- /dev/null
            +++ b/new.rs
            @@ -0,0 +1 @@
            +fn g() {}
            below changed.rs:
            diff --git a/changed.rs b/changed.rs
            index 59760b5..e20511e 100644
            --- a/changed.rs
            +++ b/changed.rs
            @@ -1 +1,3 @@
            -fn f() {}
            +fn f() {
            +    g();
            +}
        "#]],
    );
}
