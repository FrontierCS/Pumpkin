//! Names the commit that Pumpkin is built from, for `runtime::BUILD`: its hash and its date, as
//! git gives them, in `PUMPKIN_COMMIT_HASH` and `PUMPKIN_COMMIT_DATE`, set on every build, and
//! both empty outside a git checkout of Pumpkin's repository, or if git can't say.

use std::path::Path;

use stdx::process::{Environment, command};

fn main() {
    // This crate is `crates/runtime`, below the root of the checkout.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let (mut hash, mut date) = (String::new(), String::new());
    // A copy of the source inside another project's checkout names no commit of Pumpkin's.
    if root.join(".git").exists() {
        // Each move of HEAD, a commit on its branch included, is logged in HEAD's reflog, which a
        // worktree has of its own. Cargo reruns the script on every build while a path it watches
        // is missing, as `logs/HEAD` is if reflogs are off.
        for name in ["HEAD", "logs/HEAD"] {
            let args = ["rev-parse", "--path-format=absolute", "--git-path", name];
            if let Some(path) = git(&root, &args) {
                println!("cargo::rerun-if-changed={path}");
            }
        }
        let commit = git(
            &root,
            &["log", "--max-count=1", "--date=short", "--format=%H %cd"],
        );
        if let Some((found_hash, found_date)) =
            commit.as_deref().and_then(|line| line.split_once(' '))
        {
            (hash, date) = (found_hash.to_owned(), found_date.to_owned());
        }
    }
    // Set even when empty, so that a variable of the same name in cargo's environment, which the
    // build would read otherwise, never reaches it.
    println!("cargo::rustc-env=PUMPKIN_COMMIT_HASH={hash}");
    println!("cargo::rustc-env=PUMPKIN_COMMIT_DATE={date}");
}

/// What git prints with `args` in `root`, trimmed, if it succeeds.
fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = command("git", Environment::Inherited)
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    output.status.success().then(|| text.trim().to_owned())
}
