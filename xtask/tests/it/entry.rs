//! `deploy/pumpkin`'s refusals, against a local repository, with a stand-in `curl` that serves a
//! file that is not rustup-init, on this machine and on another platform, with a stand-in
//! `uname`, and `cargo xtask install --root`, which the script runs.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::{env, fs};

use expect_test::{Expect, expect};
use xshell::{Cmd, Shell};

/// The script, as the repository that holds it has it.
const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/pumpkin");

/// What the stand-in `curl` does: writes a file to where `--output` names.
const CURL: &str = "#!/bin/sh\nwhile [ $# -gt 0 ]; do\n    [ \"$1\" = --output ] && out=$2\n    \
                    shift\ndone\necho not rustup-init > \"$out\"\n";

/// Runs the script, as copied into a consumer's repository beside `.pumpkinversion`, which, if
/// there is a `version`, holds it with `<commit>` replaced by the commit that tag `v1` names in a
/// local repository, beside a lock on building that a process that has ended left, on the `PATH`
/// that [`path`] makes of `uname`, with `cargo`, if given, as the script that stands in for the
/// toolchain's `cargo`, beside a `rustup` that says what it installs; checks its exit code,
/// output, the temporary files left behind, in the caller's temporary directory and in
/// `.pumpkin/tmp`, and what `.pumpkin/` holds against `expect`.
fn check(uname: Option<(&str, &str)>, version: Option<&str>, cargo: Option<&str>, expect: &Expect) {
    let held = tempfile::Builder::new()
        .prefix("pumpkin-entry-")
        .tempdir()
        .unwrap();
    let scratch = held.path();
    let (source, consumer, bin, tmp) = (
        scratch.join("source"),
        scratch.join("consumer"),
        scratch.join("bin"),
        scratch.join("tmp"),
    );
    for dir in [&source, &consumer, &bin, &tmp] {
        fs::create_dir_all(dir).unwrap();
    }
    let sh = Shell::new().unwrap();
    // A repository that git's variables name for every command here, as those that
    // `git rebase --exec` sets do, which none of the git commands here may touch.
    let decoy = scratch.join("decoy");
    fs::create_dir(&decoy).unwrap();
    sh.change_dir(&decoy);
    alone(sh.cmd("git").args(["init", "--quiet"]))
        .run()
        .unwrap();
    let git = decoy.join(".git");
    let decoyed = || (fs::read_to_string(git.join("config")).unwrap(), files(&git));
    let before = decoyed();
    sh.set_var("GIT_DIR", &git);
    sh.set_var("GIT_WORK_TREE", &decoy);
    sh.set_var("GIT_INDEX_FILE", git.join("index"));
    sh.change_dir(&source);
    alone(sh.cmd("git").args(["init", "--quiet"]))
        .run()
        .unwrap();
    let mut commit =
        alone(
            sh.cmd("git")
                .args(["commit", "--quiet", "--allow-empty", "--message", "m"]),
        );
    for who in ["AUTHOR", "COMMITTER"] {
        commit = commit.env(format!("GIT_{who}_NAME"), "a");
        commit = commit.env(format!("GIT_{who}_EMAIL"), "a@b");
    }
    commit.run().unwrap();
    alone(sh.cmd("git").args(["tag", "v1"])).run().unwrap();
    let commit = alone(sh.cmd("git").args(["rev-parse", "HEAD"]))
        .read()
        .unwrap();
    fs::copy(SCRIPT, consumer.join("pumpkin")).unwrap();
    if let Some(version) = version {
        let version = version.replace("<commit>", &commit);
        fs::write(consumer.join(".pumpkinversion"), version).unwrap();
    }
    let state = stale_lock(&sh, &consumer);
    if let Some(cargo) = cargo {
        toolchain(&state, cargo);
    }
    let path = path(uname, &bin);
    sh.change_dir(&consumer);
    let repository = format!("--pumpkin-repository=file://{}", source.display());
    let script = sh.cmd("sh").arg("pumpkin").arg(repository);
    let output = script.env("PATH", path).env("TMPDIR", &tmp);
    // As a git hook runs it: the script's own git commands must ignore these, which would send
    // them to a repository that doesn't exist.
    let poison = "/nonexistent/pumpkin-poison";
    let output = output.env("GIT_DIR", poison).env("GIT_INDEX_FILE", poison);
    let output = output.ignore_status().quiet().output().unwrap();
    assert!(
        decoyed() == before,
        "a git command changed the decoy repository"
    );
    let report = report(&output, &tmp, &state, &commit);
    let report = report.replace(&scratch.display().to_string(), "<scratch>");
    expect.assert_eq(&report.replace(&commit, "<commit>"));
}

/// What [`check`] checks of the script that ended with `output`: its exit code, its output, how
/// many files it left in `tmp`, the caller's temporary directory, and in `state`'s `tmp/`, and
/// what `state`, its `.pumpkin/`, holds, with `commit` as `<commit>`.
fn report(output: &Output, tmp: &Path, state: &Path, commit: &str) -> String {
    let left = |dir: &Path| fs::read_dir(dir).map_or(0, Iterator::count);
    format!(
        "{:?}: {}{}left in the temporary directory: {}, and in .pumpkin/tmp: {}\nin .pumpkin: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        left(tmp),
        left(&state.join("tmp")),
        listed(state, commit),
    )
}

/// The `PATH` that the script runs on, with a stand-in `curl` in `bin` first, then this
/// machine's; with `uname`, a system and machine, a stand-in `uname` in `bin` that says so.
fn path(uname: Option<(&str, &str)>, bin: &Path) -> String {
    stand_in(bin, "curl", CURL);
    if let Some((system, machine)) = uname {
        let uname = format!(
            "#!/bin/sh\ncase \"$1\" in --kernel-name) echo {system} ;; --machine) echo {machine} ;; *) exit 2 ;; esac\n"
        );
        stand_in(bin, "uname", &uname);
    }
    format!("{}:{}", bin.display(), env::var("PATH").unwrap())
}

/// Leaves in the `.pumpkin/` of `consumer`, which it returns, a lock on building whose holder has
/// ended.
fn stale_lock(sh: &Shell, consumer: &Path) -> PathBuf {
    let state = consumer.join(".pumpkin");
    fs::create_dir_all(state.join("build.lock")).unwrap();
    // The shell's own ID, once it has ended.
    let ended = sh.cmd("sh").args(["-c", "echo $$"]).read().unwrap();
    fs::write(state.join("build.lock/pid"), ended).unwrap();
    state
}

/// Puts in `state`, a `.pumpkin/`, the toolchain's `rustup`, which says what it installs, and
/// `cargo`, a script, where the script finds them once rustup is installed.
fn toolchain(state: &Path, cargo: &str) {
    let tools = state.join("cargo/bin");
    fs::create_dir_all(&tools).unwrap();
    let rustup = "#!/bin/sh\necho 'info: installing the toolchain' >&2\n";
    stand_in(&tools, "rustup", rustup);
    stand_in(&tools, "cargo", cargo);
}

/// The names in `dir`, with `commit` as `<commit>`, sorted, between spaces.
fn listed(dir: &Path, commit: &str) -> String {
    let mut names: Vec<String> = (fs::read_dir(dir).unwrap())
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .map(|name| name.replace(commit, "<commit>"))
        .collect();
    names.sort();
    names.join(" ")
}

/// Writes `script` as the executable `name` in `bin`.
fn stand_in(bin: &Path, name: &str, script: &str) {
    fs::write(bin.join(name), script).unwrap();
    fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn refusals() {
    check(
        None,
        None,
        None,
        &expect![[r#"
            Some(1): pumpkin: there is no .pumpkinversion beside this script: write Pumpkin's tag, then its commit
            left in the temporary directory: 0, and in .pumpkin/tmp: 0
            in .pumpkin: build.lock"#]],
    );
    check(
        None,
        Some(""),
        None,
        &expect![[r#"
            Some(1): pumpkin: .pumpkinversion does not start with a tag: write Pumpkin's tag, then its commit
            left in the temporary directory: 0, and in .pumpkin/tmp: 0
            in .pumpkin: build.lock"#]],
    );
    check(
        None,
        Some("v1\n"),
        None,
        &expect![[r#"
            Some(1): pumpkin: .pumpkinversion does not name a commit after the tag: write the tag's full 40-digit hexadecimal commit
            left in the temporary directory: 0, and in .pumpkin/tmp: 0
            in .pumpkin: build.lock"#]],
    );
    check(
        None,
        Some("v1 abc123\n"),
        None,
        &expect![[r#"
            Some(1): pumpkin: .pumpkinversion does not name a commit after the tag: write the tag's full 40-digit hexadecimal commit
            left in the temporary directory: 0, and in .pumpkin/tmp: 0
            in .pumpkin: build.lock"#]],
    );
    check(
        None,
        Some("v1 0123456789012345678901234567890123456789\n"),
        None,
        &expect![[r#"
            Some(1): pumpkin: building Pumpkin v1 (0123456789012345678901234567890123456789) from file://<scratch>/source ...
            pumpkin: tag v1 of file://<scratch>/source is commit <commit>, not 0123456789012345678901234567890123456789: fix .pumpkinversion, or the tag if it moved
            left in the temporary directory: 0, and in .pumpkin/tmp: 0
            in .pumpkin: .gitignore tmp"#]],
    );
    check(
        None,
        Some("v2 <commit>\n"),
        None,
        &expect![[r#"
            Some(128): pumpkin: building Pumpkin v2 (<commit>) from file://<scratch>/source ...
            warning: Could not find remote branch v2 to clone.
            fatal: Remote branch v2 not found in upstream origin
            left in the temporary directory: 0, and in .pumpkin/tmp: 0
            in .pumpkin: .gitignore tmp"#]],
    );
    check(
        None,
        Some("v1 <commit>\n"),
        None,
        &expect![[r#"
            Some(1): pumpkin: building Pumpkin v1 (<commit>) from file://<scratch>/source ...
            pumpkin: downloading rustup-init 1.29.2 ...
            pumpkin: https://static.rust-lang.org/rustup/archive/1.29.2/x86_64-unknown-linux-gnu/rustup-init does not have the SHA-256 checksum 5be191e8c70a5007074254cb74d78dbfdd25cd5a61c27de7b8e5410122ec24f4: the download is not rustup-init 1.29.2
            left in the temporary directory: 0, and in .pumpkin/tmp: 0
            in .pumpkin: .gitignore tmp"#]],
    );
}

#[test]
fn other_platform() {
    check(
        Some(("Linux", "aarch64")),
        Some("v1 <commit>\n"),
        None,
        &expect![[r#"
            Some(1): pumpkin: this is Linux aarch64: run it on Linux x86_64
            left in the temporary directory: 0, and in .pumpkin/tmp: 0
            in .pumpkin: build.lock"#]],
    );
}

#[test]
fn build_output() {
    // As `cargo xtask install --root <stage>` is run, its output that of a build.
    let built = "#!/bin/sh\necho '   Compiling pumpkin' >&2\nmkdir --parents \"$4/bin\"\n\
                 printf '#!/bin/sh\\necho ran pumpkin\\n' > \"$4/bin/pumpkin\"\n\
                 chmod +x \"$4/bin/pumpkin\"\necho \"$4/bin/pumpkin\"\n";
    check(
        None,
        Some("v1 <commit>\n"),
        Some(built),
        &expect![[r#"
            Some(0): ran pumpkin
            pumpkin: building Pumpkin v1 (<commit>) from file://<scratch>/source ...
            left in the temporary directory: 0, and in .pumpkin/tmp: 0
            in .pumpkin: .gitignore <commit> cargo tmp"#]],
    );
    let broken =
        "#!/bin/sh\necho '   Compiling pumpkin' >&2\necho 'error: it broke' >&2\nexit 101\n";
    check(
        None,
        Some("v1 <commit>\n"),
        Some(broken),
        &expect![[r#"
        Some(101): pumpkin: building Pumpkin v1 (<commit>) from file://<scratch>/source ...
        info: installing the toolchain
           Compiling pumpkin
        error: it broke
        left in the temporary directory: 0, and in .pumpkin/tmp: 0
        in .pumpkin: .gitignore cargo tmp"#]],
    );
}

#[test]
#[ignore = "builds the static sandboxd: run cargo xtask ci --docker"]
fn install_root() {
    let held = tempfile::Builder::new()
        .prefix("pumpkin-install-")
        .tempdir()
        .unwrap();
    let root = held.path();
    let sh = Shell::new().unwrap();
    let printed = sh
        .cmd(env!("CARGO_BIN_EXE_xtask"))
        .arg("install")
        .arg("--root")
        .arg(root);
    let printed = printed
        .read()
        .unwrap()
        .replace(&root.display().to_string(), "<root>")
        + "\n";
    expect![[r#"
        <root>/bin/pumpkin
        <root>/bin/buildifier
        <root>/bin/ec2-boot
        <root>/bin/sandboxd
        <root>/bin/pumpkin-modal.py
        <root>/bin/pumpkin-modal.py.lock
        "#]]
    .assert_eq(&printed);
}

/// The paths of the files below `dir`, relative to it, sorted.
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut dirs = vec![dir.to_owned()];
    while let Some(next) = dirs.pop() {
        for entry in fs::read_dir(next).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                found.push(path.strip_prefix(dir).unwrap().to_owned());
            }
        }
    }
    found.sort();
    found
}

/// `command` run alone with git: with no configuration of the machine's, and none of git's
/// variables, this process's or those its `Shell` sets, which would send it to another
/// repository, as the `GIT_DIR` that `git rebase --exec` sets does.
fn alone(command: Cmd<'_>) -> Cmd<'_> {
    let inherited = env::vars_os().map(|(name, _)| name);
    let inherited = inherited.filter(|name| name.as_encoded_bytes().starts_with(b"GIT_"));
    let set = [
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
    ];
    let names = inherited.chain(set.map(OsString::from));
    let command = names.fold(command, Cmd::env_remove);
    let command = command.env("GIT_CONFIG_GLOBAL", "/dev/null");
    command.env("GIT_CONFIG_SYSTEM", "/dev/null")
}
