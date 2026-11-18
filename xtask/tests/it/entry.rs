//! `deploy/pumpkin`'s refusals, against a local repository, with a stand-in `curl` that serves a
//! file that is not rustup-init, on this machine, on one that lacks what installing needs and on
//! another platform, with a stand-in `uname`; its download of a release, with this machine's
//! `curl` from a directory of releases, and `cargo xtask install --root`, which the script runs.

use std::ffi::OsString;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::{env, fs};

use expect_test::{Expect, expect};
use xshell::{Cmd, Shell};

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/pumpkin");

/// What the stand-in `curl` does: writes a file to where `--output` names.
const CURL: &str = "#!/bin/sh\nwhile [ $# -gt 0 ]; do\n    [ \"$1\" = --output ] && out=$2\n    \
                    shift\ndone\necho not rustup-init > \"$out\"\n";

/// Where the script runs.
#[derive(Clone, Copy)]
enum Machine<'a> {
    This,
    /// A stand-in `uname` names this system and machine.
    Other(&'a str, &'a str),
    /// The `PATH` holds only what the script runs before it checks for what installing needs, and
    /// the script builds, or, if `release`, downloads a release.
    Lacking {
        release: bool,
    },
    /// The repository that the script fetches from is not there.
    Unreachable,
    /// `curl` fails as it does with no network.
    Offline,
    /// The script downloads the release `v1`, with this machine's `curl`, from a directory.
    Release(Archive),
}

/// The archive of the release `v1` that the directory of releases holds.
#[derive(Clone, Copy)]
enum Archive {
    /// Its `bin/pumpkin` names the commit of `v1`.
    Named,
    /// Its `bin/pumpkin` names another commit.
    Misnamed,
    /// There is none.
    Absent,
}

/// Runs the script, as copied into a consumer's repository beside `.pumpkinversion`, which, if
/// there is a `version`, holds it with `<commit>` replaced by the commit that tag `v1` names in a
/// local repository, beside a lock on building and a build's scratch directory that a process
/// that has ended left, on the `PATH` that [`path`] makes for `machine`, with `cargo`, if given,
/// as the script that stands in for the toolchain's `cargo`, beside a `rustup` that says what it
/// installs; checks its exit code, output, the temporary files left behind, in the caller's
/// temporary directory and in `.pumpkin/tmp`, and what `.pumpkin/` holds against `expect`. A
/// `<checksum>` in `version` is the checksum of the archive that `machine` downloads.
fn check(machine: Machine<'_>, version: Option<&str>, cargo: Option<&str>, expect: &Expect) {
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
    let commit = commit_in(&sh, &source);
    let checksum = match machine {
        Machine::Release(archive) => pack(&sh, scratch, archive, &commit),
        _ => "0".repeat(64),
    };
    let version = version.map(|version| version.replace("<commit>", &commit));
    consumer_files(machine, scratch, version.as_deref(), &checksum);
    let state = stale_lock(&sh, &consumer);
    if let Some(cargo) = cargo {
        toolchain(&state, cargo);
    }
    let path = path(machine, &bin);
    sh.change_dir(&consumer);
    let repository = repository(machine, &source);
    let script = sh.cmd("sh").arg("pumpkin").args(repository);
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
    let report = report.replace(&checksum, "<checksum>");
    let report = report.replace(&commit, "<commit>");
    expect.assert_eq(&report.replace(&commit[..9], "<digits>"));
}

/// Makes `source` a repository of one empty commit tagged `v1`, and returns the commit.
fn commit_in(sh: &Shell, source: &Path) -> String {
    sh.change_dir(source);
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
    alone(sh.cmd("git").args(["rev-parse", "HEAD"]))
        .read()
        .unwrap()
}

/// Writes into `<scratch>/consumer` the script and, if there is a `version`, `.pumpkinversion`
/// with it, `<checksum>` replaced by `checksum`. For a release, the script has its releases in
/// `<scratch>/releases`, which `curl` reads as files, in place of GitHub's.
fn consumer_files(machine: Machine<'_>, scratch: &Path, version: Option<&str>, checksum: &str) {
    let consumer = scratch.join("consumer");
    if let Some(version) = version {
        let version = version.replace("<checksum>", checksum);
        fs::write(consumer.join(".pumpkinversion"), version).unwrap();
    }
    let mut text = fs::read_to_string(SCRIPT).unwrap();
    if let Machine::Release(_) = machine {
        let releases = "https://github.com/FrontierCS/Pumpkin/releases/download";
        for (from, to) in [
            (releases, format!("file://{}/releases", scratch.display())),
            ("'=https'", "'=file'".to_owned()),
        ] {
            assert!(text.contains(from), "the script lacks {from}");
            text = text.replace(from, &to);
        }
    }
    fs::write(consumer.join("pumpkin"), text).unwrap();
}

/// Puts the release `v1`'s archive, of the kind `archive`, in `<scratch>/releases/v1`, and returns
/// its checksum. Its `bin/pumpkin`, a script, prints a version that names `commit`, by its first
/// 9 digits as Pumpkin does, or another.
fn pack(sh: &Shell, scratch: &Path, archive: Archive, commit: &str) -> String {
    let named = match archive {
        Archive::Named => &commit[..9],
        Archive::Misnamed => &"0".repeat(9),
        Archive::Absent => return "0".repeat(64),
    };
    let (packed, tree) = (scratch.join("releases/v1"), scratch.join("tree"));
    fs::create_dir_all(&packed).unwrap();
    fs::create_dir_all(tree.join("bin")).unwrap();
    stand_in(
        &tree.join("bin"),
        "pumpkin",
        &format!("#!/bin/sh\necho 'pumpkin 0.1.0 ({named})'\n"),
    );
    fs::write(tree.join("pumpkin.bundle"), "").unwrap();
    let file = packed.join("pumpkin-v1-x86_64-unknown-linux-musl.tar.gz");
    let tar = sh.cmd("tar").args(["--create", "--gzip", "--file"]);
    tar.arg(&file)
        .arg("--directory")
        .arg(&tree)
        .args(["bin", "pumpkin.bundle"])
        .run()
        .unwrap();
    let sum = sh.cmd("sha256sum").arg(&file).read().unwrap();
    sum.split_whitespace().next().unwrap().to_owned()
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

/// The script's option that names `source`, the local repository, as where it fetches from, but
/// on a machine that can't reach it, a path beside it that holds nothing; none for a release.
fn repository(machine: Machine<'_>, source: &Path) -> Option<String> {
    let source = match machine {
        Machine::Release(_) | Machine::Lacking { release: true } => return None,
        Machine::Unreachable => source.with_file_name("nonexistent"),
        Machine::This | Machine::Other(..) | Machine::Lacking { .. } | Machine::Offline => {
            source.to_owned()
        }
    };
    Some(format!("--pumpkin-repository=file://{}", source.display()))
}

/// The `PATH` that the script runs on, on `machine`: `bin`, holding a stand-in `curl` and what
/// `machine` asks for, then this machine's; on one that is lacking, `bin` alone.
fn path(machine: Machine<'_>, bin: &Path) -> String {
    let path = env::var("PATH").unwrap();
    match machine {
        Machine::Lacking { .. } => {
            for tool in ["sh", "uname", "dirname"] {
                let mut found = env::split_paths(&path).map(|dir| dir.join(tool));
                let found = found.find(|tool| tool.is_file()).unwrap();
                symlink(found, bin.join(tool)).unwrap();
            }
            return bin.display().to_string();
        }
        Machine::Other(system, machine) => {
            let uname = format!(
                "#!/bin/sh\ncase \"$1\" in --kernel-name) echo {system} ;; --machine) echo {machine} ;; *) exit 2 ;; esac\n"
            );
            stand_in(bin, "uname", &uname);
        }
        Machine::Offline => {
            let curl = "#!/bin/sh\necho 'curl: (6) Could not resolve host: static.rust-lang.org' >&2\nexit 6\n";
            stand_in(bin, "curl", curl);
            return format!("{}:{path}", bin.display());
        }
        Machine::Release(_) => return path,
        Machine::This | Machine::Unreachable => {}
    }
    stand_in(bin, "curl", CURL);
    format!("{}:{path}", bin.display())
}

/// Leaves in the `.pumpkin/` of `consumer`, which it returns, a lock on building whose holder has
/// ended, and that holder's scratch directory.
fn stale_lock(sh: &Shell, consumer: &Path) -> PathBuf {
    let state = consumer.join(".pumpkin");
    fs::create_dir_all(state.join("build.lock")).unwrap();
    fs::create_dir_all(state.join("tmp/build.ended")).unwrap();
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

fn stand_in(bin: &Path, name: &str, script: &str) {
    fs::write(bin.join(name), script).unwrap();
    fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn refusals() {
    check(
        Machine::This,
        None,
        None,
        &expect![[r#"
            Some(1): pumpkin: there is no .pumpkinversion beside this script: write Pumpkin's tag, then its commit
            left in the temporary directory: 0, and in .pumpkin/tmp: 1
            in .pumpkin: build.lock tmp"#]],
    );
    check(
        Machine::This,
        Some(""),
        None,
        &expect![[r#"
            Some(1): pumpkin: .pumpkinversion does not start with a tag: write Pumpkin's tag, then its commit
            left in the temporary directory: 0, and in .pumpkin/tmp: 1
            in .pumpkin: build.lock tmp"#]],
    );
    check(
        Machine::This,
        Some("v1\n"),
        None,
        &expect![[r#"
            Some(1): pumpkin: .pumpkinversion does not name a commit after the tag: write the tag's full 40-digit hexadecimal commit
            left in the temporary directory: 0, and in .pumpkin/tmp: 1
            in .pumpkin: build.lock tmp"#]],
    );
    check(
        Machine::This,
        Some("v1 abc123\n"),
        None,
        &expect![[r#"
            Some(1): pumpkin: .pumpkinversion does not name a commit after the tag: write the tag's full 40-digit hexadecimal commit
            left in the temporary directory: 0, and in .pumpkin/tmp: 1
            in .pumpkin: build.lock tmp"#]],
    );
    check(
        Machine::This,
        Some("v1 <commit> 0123abcd\n"),
        None,
        &expect![[r#"
            Some(1): pumpkin: .pumpkinversion's third word is not a SHA-256 checksum: write the 64-digit hexadecimal checksum from the .sha256 beside the release's archive, or no third word to build from source
            left in the temporary directory: 0, and in .pumpkin/tmp: 1
            in .pumpkin: build.lock tmp"#]],
    );
}

#[test]
fn fetch_refusals() {
    check(
        Machine::This,
        Some("v1 0123456789012345678901234567890123456789\n"),
        None,
        &expect![[r#"
            Some(1): pumpkin: this first run fetches a Rust toolchain and builds Pumpkin, about 250 MB and a few minutes;
            pumpkin: all of it, about 1.5 GB, goes to <scratch>/consumer/.pumpkin, safe to delete, and later runs start at once
            pumpkin: building Pumpkin v1 (0123456789012345678901234567890123456789) from file://<scratch>/source ...
            pumpkin: tag v1 of file://<scratch>/source is commit <commit>, not 0123456789012345678901234567890123456789: fix .pumpkinversion, or the tag if it moved
            left in the temporary directory: 0, and in .pumpkin/tmp: 0
            in .pumpkin: .gitignore tmp"#]],
    );
    check(
        Machine::This,
        Some("v2 <commit>\n"),
        None,
        &expect![[r#"
            Some(128): pumpkin: this first run fetches a Rust toolchain and builds Pumpkin, about 250 MB and a few minutes;
            pumpkin: all of it, about 1.5 GB, goes to <scratch>/consumer/.pumpkin, safe to delete, and later runs start at once
            pumpkin: building Pumpkin v2 (<commit>) from file://<scratch>/source ...
            warning: Could not find remote branch v2 to clone.
            fatal: Remote branch v2 not found in upstream origin
            pumpkin: file://<scratch>/source has no tag v2: name one of its tags in .pumpkinversion; list them with
                git ls-remote --tags file://<scratch>/source
            left in the temporary directory: 0, and in .pumpkin/tmp: 0
            in .pumpkin: .gitignore tmp"#]],
    );
    check(
        Machine::This,
        Some("v1 <commit>\n"),
        None,
        &expect![[r#"
            Some(1): pumpkin: this first run fetches a Rust toolchain and builds Pumpkin, about 250 MB and a few minutes;
            pumpkin: all of it, about 1.5 GB, goes to <scratch>/consumer/.pumpkin, safe to delete, and later runs start at once
            pumpkin: building Pumpkin v1 (<commit>) from file://<scratch>/source ...
            pumpkin: downloading rustup-init 1.29.2 ...
            pumpkin: https://static.rust-lang.org/rustup/archive/1.29.2/x86_64-unknown-linux-gnu/rustup-init does not have the SHA-256 checksum 5be191e8c70a5007074254cb74d78dbfdd25cd5a61c27de7b8e5410122ec24f4: the download is not rustup-init 1.29.2
            left in the temporary directory: 0, and in .pumpkin/tmp: 0
            in .pumpkin: .gitignore tmp"#]],
    );
}

#[test]
fn other_platform() {
    check(
        Machine::Other("Linux", "aarch64"),
        Some("v1 <commit>\n"),
        None,
        &expect![[r#"
            Some(1): pumpkin: this is Linux aarch64: run it on Linux x86_64
            left in the temporary directory: 0, and in .pumpkin/tmp: 1
            in .pumpkin: build.lock tmp"#]],
    );
}

#[test]
fn lacking() {
    check(
        Machine::Lacking { release: false },
        Some("v1 <commit>\n"),
        None,
        &expect![[r#"
            Some(1): pumpkin: building Pumpkin needs git, curl or wget, sha256sum, cc (a C linker), which this machine lacks; on Debian or Ubuntu, run
                sudo apt-get install git curl coreutils gcc libc6-dev
            left in the temporary directory: 0, and in .pumpkin/tmp: 1
            in .pumpkin: build.lock tmp"#]],
    );
}

#[test]
fn unreachable() {
    check(
        Machine::Unreachable,
        Some("v1 <commit>\n"),
        None,
        &expect![[r#"
        Some(128): pumpkin: this first run fetches a Rust toolchain and builds Pumpkin, about 250 MB and a few minutes;
        pumpkin: all of it, about 1.5 GB, goes to <scratch>/consumer/.pumpkin, safe to delete, and later runs start at once
        pumpkin: building Pumpkin v1 (<commit>) from file://<scratch>/nonexistent ...
        fatal: '<scratch>/nonexistent' does not appear to be a git repository
        fatal: Could not read from remote repository.

        Please make sure you have the correct access rights
        and the repository exists.
        pumpkin: could not read the repository file://<scratch>/nonexistent: it needs the network, and access to it on GitHub; check both with
            git ls-remote file://<scratch>/nonexistent
        left in the temporary directory: 0, and in .pumpkin/tmp: 0
        in .pumpkin: .gitignore tmp"#]],
    );
}

#[test]
fn offline() {
    check(
        Machine::Offline,
        Some("v1 <commit>\n"),
        None,
        &expect![[r#"
            Some(6): pumpkin: this first run fetches a Rust toolchain and builds Pumpkin, about 250 MB and a few minutes;
            pumpkin: all of it, about 1.5 GB, goes to <scratch>/consumer/.pumpkin, safe to delete, and later runs start at once
            pumpkin: building Pumpkin v1 (<commit>) from file://<scratch>/source ...
            pumpkin: downloading rustup-init 1.29.2 ...
            curl: (6) Could not resolve host: static.rust-lang.org
            pumpkin: the first run downloads from static.rust-lang.org, index.crates.io, static.crates.io, github.com and release-assets.githubusercontent.com; check that this machine reaches them with
                curl --head https://static.rust-lang.org/rustup/archive/1.29.2/x86_64-unknown-linux-gnu/rustup-init
            left in the temporary directory: 0, and in .pumpkin/tmp: 0
            in .pumpkin: .gitignore tmp"#]],
    );
}

#[test]
fn lacking_for_release() {
    check(
        Machine::Lacking { release: true },
        Some("v1 <commit> <checksum>\n"),
        None,
        &expect![[r#"
            Some(1): pumpkin: downloading Pumpkin needs curl or wget, sha256sum, tar, which this machine lacks; on Debian or Ubuntu, run
                sudo apt-get install curl coreutils tar
            left in the temporary directory: 0, and in .pumpkin/tmp: 1
            in .pumpkin: build.lock tmp"#]],
    );
}

#[test]
fn release() {
    check(
        Machine::Release(Archive::Named),
        Some("v1 <commit> <checksum>\n"),
        None,
        &expect![[r#"
            Some(0): pumpkin 0.1.0 (<digits>)
            pumpkin: downloading Pumpkin v1 (<commit>) from file://<scratch>/releases/v1/pumpkin-v1-x86_64-unknown-linux-musl.tar.gz ...
            left in the temporary directory: 0, and in .pumpkin/tmp: 0
            in .pumpkin: .gitignore <commit> tmp"#]],
    );
}

#[test]
fn release_checksum() {
    let pinned = "0".repeat(64);
    check(
        Machine::Release(Archive::Named),
        Some(&format!("v1 <commit> {pinned}\n")),
        None,
        &expect![[r#"
            Some(1): pumpkin: downloading Pumpkin v1 (<commit>) from file://<scratch>/releases/v1/pumpkin-v1-x86_64-unknown-linux-musl.tar.gz ...
            pumpkin: file://<scratch>/releases/v1/pumpkin-v1-x86_64-unknown-linux-musl.tar.gz has the SHA-256 checksum <checksum>, not 0000000000000000000000000000000000000000000000000000000000000000, which .pumpkinversion pins: if the release was rebuilt on purpose, pin the checksum in file://<scratch>/releases/v1/pumpkin-v1-x86_64-unknown-linux-musl.tar.gz.sha256 instead; otherwise do not run this download
            left in the temporary directory: 0, and in .pumpkin/tmp: 0
            in .pumpkin: .gitignore tmp"#]],
    );
}

#[test]
fn release_commit() {
    check(
        Machine::Release(Archive::Misnamed),
        Some("v1 <commit> <checksum>\n"),
        None,
        &expect![[r#"
            Some(1): pumpkin: downloading Pumpkin v1 (<commit>) from file://<scratch>/releases/v1/pumpkin-v1-x86_64-unknown-linux-musl.tar.gz ...
            pumpkin: the pumpkin in file://<scratch>/releases/v1/pumpkin-v1-x86_64-unknown-linux-musl.tar.gz says 'pumpkin 0.1.0 (000000000)', which does not name commit <commit>: fix the tag or the commit in .pumpkinversion, or the release if it was built from another commit
            left in the temporary directory: 0, and in .pumpkin/tmp: 0
            in .pumpkin: .gitignore tmp"#]],
    );
}

#[test]
fn release_absent() {
    check(
        Machine::Release(Archive::Absent),
        Some("v1 <commit> <checksum>\n"),
        None,
        &expect![[r#"
            Some(37): pumpkin: downloading Pumpkin v1 (<commit>) from file://<scratch>/releases/v1/pumpkin-v1-x86_64-unknown-linux-musl.tar.gz ...
            curl: (37) Couldn't open file <scratch>/releases/v1/pumpkin-v1-x86_64-unknown-linux-musl.tar.gz
            pumpkin: could not download file://<scratch>/releases/v1/pumpkin-v1-x86_64-unknown-linux-musl.tar.gz: check that this machine reaches github.com and release-assets.githubusercontent.com, and that tag v1 has a release at https://github.com/FrontierCS/Pumpkin/releases
            left in the temporary directory: 0, and in .pumpkin/tmp: 0
            in .pumpkin: .gitignore tmp"#]],
    );
}

/// What the stand-in `cargo` does as `cargo xtask install --root <stage>` is run: its output that
/// of a build, and a `bin/pumpkin` in the stage.
const BUILT: &str = "#!/bin/sh\necho '   Compiling pumpkin' >&2\nmkdir --parents \"$4/bin\"\n\
                     printf '#!/bin/sh\\necho ran pumpkin\\n' > \"$4/bin/pumpkin\"\n\
                     chmod +x \"$4/bin/pumpkin\"\necho \"$4/bin/pumpkin\"\n";

#[test]
fn build_with_checksum() {
    // The option asks for a build from source, whatever `.pumpkinversion` pins.
    check(
        Machine::This,
        Some("v1 <commit> <checksum>\n"),
        Some(BUILT),
        &expect![[r#"
            Some(0): ran pumpkin
            pumpkin: building Pumpkin v1 (<commit>) from file://<scratch>/source ...
            left in the temporary directory: 0, and in .pumpkin/tmp: 0
            in .pumpkin: .gitignore <commit> cargo tmp"#]],
    );
}

#[test]
fn build_output() {
    check(
        Machine::This,
        Some("v1 <commit>\n"),
        Some(BUILT),
        &expect![[r#"
            Some(0): ran pumpkin
            pumpkin: building Pumpkin v1 (<commit>) from file://<scratch>/source ...
            left in the temporary directory: 0, and in .pumpkin/tmp: 0
            in .pumpkin: .gitignore <commit> cargo tmp"#]],
    );
    let broken =
        "#!/bin/sh\necho '   Compiling pumpkin' >&2\necho 'error: it broke' >&2\nexit 101\n";
    check(
        Machine::This,
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
#[ignore = "builds the static sandboxd: run cargo xtask live docker"]
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
