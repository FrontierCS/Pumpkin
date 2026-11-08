//! xtask's checks of Rust files ([`tidy`]), run on a scratch Git repository. xtask is a binary,
//! so its test binary compiles the checks' module itself.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::error::Error;
use std::fs;
use std::path::PathBuf;

use expect_test::{Expect, expect};
use xshell::{Shell, cmd};

#[path = "../src/tidy.rs"]
mod tidy;

/// The result the check returns, as xtask's tasks do.
type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// How a file stands in the scratch repository.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Standing {
    Tracked,
    Untracked,
    /// In a directory the repository ignores.
    Ignored,
    /// Tracked, then deleted from the working tree.
    Deleted,
}

/// A scratch directory, which is removed as it drops, so also when a test fails.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        // A directory already gone is fine.
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Writes `files`, each a path, its text and how it stands, into a new Git repository, runs the
/// check there, and checks what it reports against `expect`.
fn check(name: &str, files: &[(&str, String, Standing)], expect: &Expect) {
    let dir = std::env::temp_dir().join(format!("pumpkin-xtask-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let _scratch = Scratch(dir.clone());
    let sh = Shell::new().unwrap();
    sh.change_dir(&dir);
    // Git's own variables, inherited, would steer every command here to another repository, as
    // the `GIT_DIR` that `git rebase --exec` sets in a linked worktree would. A `Shell` can set
    // a variable but not remove one, so each that places a repository is set to this one's.
    let git = dir.join(".git");
    sh.set_var("GIT_DIR", &git);
    sh.set_var("GIT_COMMON_DIR", &git);
    sh.set_var("GIT_WORK_TREE", &dir);
    sh.set_var("GIT_INDEX_FILE", git.join("index"));
    sh.set_var("GIT_OBJECT_DIRECTORY", git.join("objects"));
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
    let reported = match tidy::check(&sh) {
        Ok(()) => "passed".to_owned(),
        Err(error) => error.to_string(),
    };
    expect.assert_eq(&reported);
}

/// `count` lines, each `line`.
fn lines(line: &str, count: usize) -> String {
    format!("{line}\n").repeat(count)
}

/// Docs at their caps pass, and docs a line over fail: a module doc behind a multi-line inner
/// attribute or a block comment, or in parts, an item's doc split by attributes, blank lines
/// and comments, docs in bodies (a method's, an inline module's, and one in a macro's rule),
/// and those of a file whose name Git would quote and of an untracked file. Lines in raw
/// strings, an ignored file and a deleted one aren't counted.
#[test]
fn caps() {
    use Standing::{Deleted, Ignored, Tracked, Untracked};
    let at_caps = lines("//! A line.", 40) + &lines("/// A line.", 15) + "fn f() {}\n";
    let behind_attribute = format!(
        "#![expect(\n    dead_code,\n    reason = \"a reason\"\n)]\n{}",
        lines("//! A line.", 41)
    );
    let in_parts = format!(
        "/* A comment. */\n{}\n#![doc = \"A line.\"]\n{}",
        lines("//! A line.", 30),
        lines("//! A line.", 10)
    );
    let item_in_parts = format!(
        "{}#[derive(Debug)]\n\n// A comment.\n{}#[doc = \"A line.\"]\nstruct S;\n",
        lines("/// A line.", 8),
        lines("/// A line.", 7)
    );
    let raw = format!(
        "const S: &str = r#\"\n{}\"#;\nconst B: &[u8] = br\"\n{}\";\nfn f() {{}}\n",
        lines("/// Not a doc.", 20),
        lines("//! Not a doc.", 41)
    );
    let separate =
        lines("/// A line.", 15) + "fn a() {}\n" + &lines("/// A line.", 15) + "fn b() {}\n";
    let long_item = lines("/// A line.", 16) + "fn f() {}\n";
    let method = format!(
        "struct S;\nimpl S {{\n{}    fn f() {{}}\n}}\n",
        lines("    /// A line.", 16)
    );
    let inline_module = format!("mod m {{\n{}}}\n", lines("    //! A line.", 41));
    let macro_rule = format!(
        "macro_rules! m {{\n    () => {{\n{}        fn f() {{}}\n    }};\n}}\n",
        lines("        /// A line.", 16)
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
        &expect![[r#"
            untracked.rs:1: an item's doc is 16 lines, over the cap of 15: shorten it to 15 or fewer, moving each fact to its home
            behind_attribute.rs:5: the module doc is 41 lines, over the cap of 40: shorten it to 40 or fewer, moving each fact to its home
            in_parts.rs:2: the module doc is 41 lines, over the cap of 40: shorten it to 40 or fewer, moving each fact to its home
            inline_module.rs:2: the module doc is 41 lines, over the cap of 40: shorten it to 40 or fewer, moving each fact to its home
            item_in_parts.rs:1: an item's doc is 16 lines, over the cap of 15: shorten it to 15 or fewer, moving each fact to its home
            macro_rule.rs:3: an item's doc is 16 lines, over the cap of 15: shorten it to 15 or fewer, moving each fact to its home
            method.rs:3: an item's doc is 16 lines, over the cap of 15: shorten it to 15 or fewer, moving each fact to its home
            é.rs:1: an item's doc is 16 lines, over the cap of 15: shorten it to 15 or fewer, moving each fact to its home"#]],
    );
}

/// A doc comment's example may hold a raw string, and a doc comment's lines count however they
/// read; strings, of any kind, hold no doc, however their lines read. A block doc comment spans
/// its lines, as a multi-line `doc` attribute does, and a `doc` attribute that holds no string
/// isn't counted. A file that isn't valid Rust is reported where its tokens break.
#[test]
fn forms() {
    use Standing::Tracked;
    let example = format!(
        "/// ```\n/// let s = r#\"\n{}/// \"#;\n/// ```\nfn f() {{}}\n",
        lines("/// A line.", 12)
    );
    let strings = format!(
        "const A: &str = \"-r\";\nconst B: &str = \"r#\";\nconst C: &CStr = cr\"\n{}\";\n\
         const D: &str = \"\n{}\";\n{}fn f() {{}}\n",
        lines("/// Not a doc.", 16),
        lines("//! Not a doc.", 41),
        lines("/// A line.", 16)
    );
    let bracket = format!(
        "#[expect(\n    dead_code,\n    reason = \"a [bracket\"\n)]\nfn a() {{}}\n{}fn b() {{}}\n",
        lines("/// A line.", 16)
    );
    let blocks = format!(
        "/*!\n{}*/\n/**\n{}*/\nfn f() {{}}\n",
        lines(" A line.", 39),
        lines(" A line.", 14)
    );
    let attributes = format!(
        "#[doc = \"A line.\n{}\"]\nfn f() {{}}\n{}#[doc(hidden)]\nfn g() {{}}\n",
        lines("A line.", 14),
        lines("/// A line.", 15)
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
        &expect![[r#"
            attributes.rs:1: an item's doc is 16 lines, over the cap of 15: shorten it to 15 or fewer, moving each fact to its home
            blocks.rs:1: the module doc is 41 lines, over the cap of 40: shorten it to 40 or fewer, moving each fact to its home
            blocks.rs:42: an item's doc is 16 lines, over the cap of 15: shorten it to 15 or fewer, moving each fact to its home
            bracket.rs:6: an item's doc is 16 lines, over the cap of 15: shorten it to 15 or fewer, moving each fact to its home
            example.rs:1: an item's doc is 16 lines, over the cap of 15: shorten it to 15 or fewer, moving each fact to its home
            invalid.rs:2:13: the file is not valid Rust here, so it can't be checked: make it valid Rust
            strings.rs:64: an item's doc is 16 lines, over the cap of 15: shorten it to 15 or fewer, moving each fact to its home"#]],
    );
}

/// An `unreachable!` without a message and any `debug_assert!` fail, in any delimiters, by their
/// paths, and in a body or a macro's rule, and are reported beside a long doc in line order; an
/// `unreachable!` with a message and `assert!` pass, as do the names in a string, a comment or
/// another identifier, and a call of a function so named.
#[test]
fn unreachable() {
    use Standing::Tracked;
    let bare = format!(
        "fn f() {{\n    unreachable!()\n}}\n{}fn g() {{\n    std::unreachable![];\n}}\n\
         macro_rules! m {{\n    () => {{ core::unreachable! {{}} }};\n}}\n\
         fn h(x: u8) {{\n    debug_assert!(x > 0);\n    std::debug_assert_eq!(x, 1, \"why\");\n    \
         debug_assert_ne![x, 2];\n}}\n",
        lines("/// A line.", 16)
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
        &expect![[r#"
            bare.rs:2:5: `unreachable!` has no message: say in one why only a bug can reach it
            bare.rs:4: an item's doc is 16 lines, over the cap of 15: shorten it to 15 or fewer, moving each fact to its home
            bare.rs:21:10: `unreachable!` has no message: say in one why only a bug can reach it
            bare.rs:24:19: `unreachable!` has no message: say in one why only a bug can reach it
            bare.rs:27:5: `debug_assert!` is skipped by release builds: use `assert!`, which guards the real run too
            bare.rs:28:10: `debug_assert_eq!` is skipped by release builds: use `assert_eq!`, which guards the real run too
            bare.rs:29:5: `debug_assert_ne!` is skipped by release builds: use `assert_ne!`, which guards the real run too"#]],
    );
}
