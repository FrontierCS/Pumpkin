//! The release launcher against pinned archives read through curl, and the static install.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use expect_test::{Expect, expect};
use sha2::{Digest as _, Sha256};
use stdx::process::{self, Environment};
use test_support::TestDir;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../dist/pumpkin");
const RELEASES: &str = "https://github.com/FrontierCS/Pumpkin/releases/download";
const COMMIT: &str = "1234567890123456789012345678901234567890";
const WRONG_COMMIT: &str = "1234567890123456789012345678901234567891";
const WRONG_SHA: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Clone, Copy)]
enum Release {
    None,
    WrongCommit,
    Current,
    Cached,
    Concurrent,
}

fn check(release: Release, pin: Option<&str>, lacking: bool, expect: &Expect) {
    let held = TestDir::new();
    let fixture = Fixture::new(held.path(), release, lacking);
    let output = match release {
        Release::Cached => {
            fixture.warm_cache();
            fixture.run("repo", pin)
        }
        Release::Concurrent => fixture.run_concurrent(pin),
        Release::None | Release::WrongCommit | Release::Current => fixture.run("repo", pin),
    };

    let entry = fixture.cache.join(&fixture.checksum);
    if entry.is_dir() {
        assert_eq!(listed(&entry), "bin commit");
        assert_eq!(
            fs::read_to_string(entry.join("commit")).unwrap(),
            format!("{COMMIT}\n")
        );
    }
    let report = format!(
        "{:?}: {}{}workspace files: {}\ncache entries: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        listed(&fixture.scratch.join("repo/.pumpkin")),
        listed(&fixture.cache),
    );
    let report = report.replace(&fixture.scratch.display().to_string(), "<scratch>");
    let report = match release {
        Release::None => report,
        _ => report.replace(&fixture.checksum, "<checksum>"),
    };
    expect.assert_eq(&report);
}

struct Fixture<'a> {
    scratch: &'a Path,
    cache: PathBuf,
    xdg: Option<PathBuf>,
    checksum: String,
    launcher: String,
    path: OsString,
    probes: Option<[PathBuf; 2]>,
}

impl<'a> Fixture<'a> {
    fn new(scratch: &'a Path, release: Release, lacking: bool) -> Self {
        let checksum = match release {
            Release::None => WRONG_SHA.to_owned(),
            Release::WrongCommit => archive(scratch, WRONG_COMMIT),
            Release::Current | Release::Cached | Release::Concurrent => archive(scratch, COMMIT),
        };
        let xdg = matches!(release, Release::Cached).then(|| scratch.join("cache"));
        let cache = match &xdg {
            Some(root) => root.join("pumpkin"),
            None => scratch.join("home/.cache/pumpkin"),
        };
        let launcher = fs::read_to_string(SCRIPT).unwrap();
        let launcher = if matches!(release, Release::None) {
            launcher
        } else {
            launcher
                .replace(
                    RELEASES,
                    &format!("file://{}/releases/download", scratch.display()),
                )
                .replace("'=https'", "'=file'")
        };
        let (path, probes) = launcher_path(scratch, release, lacking);
        Self {
            scratch,
            cache,
            xdg,
            checksum,
            launcher,
            path,
            probes,
        }
    }

    fn run(&self, name: &str, pin: Option<&str>) -> Output {
        let repo = self.scratch.join(name);
        let subdir = repo.join("subdir");
        fs::create_dir_all(&subdir).unwrap();
        fs::write(repo.join("pumpkin"), &self.launcher).unwrap();
        if let Some(pin) = pin {
            fs::write(
                repo.join(".pumpkinversion"),
                pin.replace("<checksum>", &self.checksum),
            )
            .unwrap();
        }
        let mut call = process::command("sh", Environment::Inherited);
        call.current_dir(subdir)
            .args(["../pumpkin", "check", "task with spaces.star"])
            .env("HOME", self.scratch.join("home"))
            .env("PATH", &self.path);
        match &self.xdg {
            Some(root) => call.env("XDG_CACHE_HOME", root),
            None => call.env_remove("XDG_CACHE_HOME"),
        };
        if let Some([flock, curl]) = &self.probes {
            call.env("PROBES", self.scratch)
                .env("REAL_FLOCK", flock)
                .env("REAL_CURL", curl);
        }
        call.output().unwrap()
    }

    fn warm_cache(&self) {
        let pin = format!("v1 {COMMIT} <checksum>");
        self.check_installed("first", &self.run("first", Some(&pin)));
        fs::remove_dir_all(self.scratch.join("releases")).unwrap();
    }

    fn run_concurrent(&self, pin: Option<&str>) -> Output {
        let locks = self.cache.join("locks");
        fs::create_dir_all(&locks).unwrap();
        let gate = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(locks.join(&self.checksum))
            .unwrap();
        gate.lock().unwrap();
        thread::scope(|scope| {
            let first = scope.spawn(|| self.run("first", pin));
            let second = scope.spawn(|| self.run("repo", pin));
            let started = Instant::now();
            let attempts = self.scratch.join("flock");
            let both_cold = loop {
                if fs::read_to_string(&attempts).is_ok_and(|text| text.lines().count() == 2) {
                    break true;
                }
                if first.is_finished()
                    || second.is_finished()
                    || started.elapsed() >= Duration::from_secs(2)
                {
                    break false;
                }
                thread::sleep(Duration::from_millis(5));
            };
            gate.unlock().unwrap();
            let first = first.join().unwrap();
            let mut second = second.join().unwrap();
            self.check_installed("first", &first);
            self.check_installed("repo", &second);
            assert!(both_cold, "both launchers must reach the cold cache lock");
            let downloads = fs::read_to_string(self.scratch.join("curl"))
                .unwrap()
                .lines()
                .count();
            second.stderr = format!("downloads: {downloads}\n").into_bytes();
            second
        })
    }

    fn check_installed(&self, name: &str, output: &Output) {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            listed(&self.scratch.join(name).join(".pumpkin")),
            ".gitignore"
        );
    }
}

fn launcher_path(
    scratch: &Path,
    release: Release,
    lacking: bool,
) -> (OsString, Option<[PathBuf; 2]>) {
    let path = env::var_os("PATH").unwrap();
    let tool = |name| {
        env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|file| file.is_file())
            .unwrap()
    };
    let bin = scratch.join("bin");
    if lacking {
        fs::create_dir(&bin).unwrap();
        for name in ["sh", "uname", "dirname"] {
            symlink(tool(name), bin.join(name)).unwrap();
        }
        return (bin.into_os_string(), None);
    }
    if !matches!(release, Release::Concurrent) {
        return (path, None);
    }
    fs::create_dir(&bin).unwrap();
    let originals = ["flock", "curl"].map(|name| {
        let probe = bin.join(name);
        fs::write(
            &probe,
            format!(
                r#"#!/bin/sh
printf '.\n' >> "$PROBES/{name}"
exec "$REAL_{tool}" "$@"
"#,
                tool = name.to_ascii_uppercase(),
            ),
        )
        .unwrap();
        fs::set_permissions(probe, fs::Permissions::from_mode(0o755)).unwrap();
        tool(name)
    });
    let path = env::join_paths(std::iter::once(bin).chain(env::split_paths(&path))).unwrap();
    (path, Some(originals))
}

fn listed(dir: &Path) -> String {
    if !dir.exists() {
        return "(none)".to_owned();
    }
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort_unstable();
    if names.is_empty() {
        "(none)".to_owned()
    } else {
        names.join(" ")
    }
}

fn archive(scratch: &Path, commit: &str) -> String {
    let file = scratch.join("releases/download/v1/pumpkin-v1-x86_64-unknown-linux-musl.tar.xz");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    let tree = scratch.join("tree");
    let bin = tree.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let pumpkin = bin.join("pumpkin");
    fs::write(
        &pumpkin,
        r#"#!/bin/sh
printf 'workspace: %s\narg: %s\narg: %s\n' "$PUMPKIN_WORKSPACE" "$1" "$2"
"#,
    )
    .unwrap();
    fs::write(tree.join("commit"), format!("{commit}\n")).unwrap();
    fs::set_permissions(pumpkin, fs::Permissions::from_mode(0o755)).unwrap();
    let tarred = process::command("tar", Environment::Inherited)
        .args(["--create", "--xz", "--file"])
        .arg(&file)
        .arg("--directory")
        .arg(tree)
        .args(["bin", "commit"])
        .output()
        .unwrap();
    assert!(
        tarred.status.success(),
        "{}",
        String::from_utf8_lossy(&tarred.stderr)
    );
    let mut hex = String::new();
    for byte in Sha256::digest(fs::read(file).unwrap()) {
        write!(hex, "{byte:02x}").unwrap();
    }
    hex
}

#[test]
fn missing_pin() {
    check(
        Release::None,
        None,
        false,
        &expect![[r#"
            Some(1): pumpkin: there is no .pumpkinversion beside this script: download pumpkinversion from https://github.com/FrontierCS/Pumpkin/releases and save it as .pumpkinversion
            workspace files: (none)
            cache entries: (none)"#]],
    );
}

#[test]
fn legacy_pin() {
    check(
        Release::None,
        Some(&format!("v1 {COMMIT}\n")),
        false,
        &expect![[r#"
            Some(1): pumpkin: .pumpkinversion has no release checksum: download pumpkinversion from https://github.com/FrontierCS/Pumpkin/releases and save it as .pumpkinversion
            workspace files: (none)
            cache entries: (none)"#]],
    );
}

#[test]
fn lacking() {
    check(
        Release::None,
        Some(&format!("v1 {COMMIT} {WRONG_SHA}")),
        true,
        &expect![[r#"
            Some(1): pumpkin: downloading Pumpkin needs curl or wget, sha256sum, tar, xz, flock, which this machine lacks; on Debian or Ubuntu, run
                sudo apt-get install curl coreutils tar xz-utils util-linux
            workspace files: (none)
            cache entries: (none)"#]],
    );
}

#[test]
fn release() {
    check(
        Release::Current,
        Some(&format!("v1 {COMMIT} <checksum>")),
        false,
        &expect![[r#"
            Some(0): workspace: <scratch>/repo/.pumpkin
            arg: check
            arg: task with spaces.star
            pumpkin: downloading Pumpkin v1 (1234567890123456789012345678901234567890) from file://<scratch>/releases/download/v1/pumpkin-v1-x86_64-unknown-linux-musl.tar.xz ...
            workspace files: .gitignore
            cache entries: <checksum> locks"#]],
    );
}

#[test]
fn release_cached() {
    check(
        Release::Cached,
        Some(&format!("v1 {COMMIT} <checksum>")),
        false,
        &expect![[r#"
            Some(0): workspace: <scratch>/repo/.pumpkin
            arg: check
            arg: task with spaces.star
            workspace files: .gitignore
            cache entries: <checksum> locks"#]],
    );
}

#[test]
fn release_concurrent() {
    check(
        Release::Concurrent,
        Some(&format!("v1 {COMMIT} <checksum>")),
        false,
        &expect![[r#"
            Some(0): workspace: <scratch>/repo/.pumpkin
            arg: check
            arg: task with spaces.star
            downloads: 1
            workspace files: .gitignore
            cache entries: <checksum> locks"#]],
    );
}

#[test]
fn release_checksum() {
    check(
        Release::Current,
        Some(&format!("v1 {COMMIT} {WRONG_SHA}")),
        false,
        &expect![[r#"
            Some(1): pumpkin: downloading Pumpkin v1 (1234567890123456789012345678901234567890) from file://<scratch>/releases/download/v1/pumpkin-v1-x86_64-unknown-linux-musl.tar.xz ...
            pumpkin: file://<scratch>/releases/download/v1/pumpkin-v1-x86_64-unknown-linux-musl.tar.xz has the SHA-256 checksum <checksum>, not 0000000000000000000000000000000000000000000000000000000000000000, which .pumpkinversion pins: compare .pumpkinversion with file://<scratch>/releases/download/v1/pumpkinversion
            workspace files: (none)
            cache entries: locks"#]],
    );
}

#[test]
fn release_commit() {
    check(
        Release::WrongCommit,
        Some(&format!("v1 {COMMIT} <checksum>")),
        false,
        &expect![[r#"
            Some(1): pumpkin: downloading Pumpkin v1 (1234567890123456789012345678901234567890) from file://<scratch>/releases/download/v1/pumpkin-v1-x86_64-unknown-linux-musl.tar.xz ...
            pumpkin: the release at file://<scratch>/releases/download/v1/pumpkin-v1-x86_64-unknown-linux-musl.tar.xz names commit 1234567890123456789012345678901234567891, not 1234567890123456789012345678901234567890: compare .pumpkinversion with file://<scratch>/releases/download/v1/pumpkinversion
            workspace files: (none)
            cache entries: locks"#]],
    );
}

#[test]
fn release_cached_commit() {
    check(
        Release::Cached,
        Some(&format!("v1 {WRONG_COMMIT} <checksum>")),
        false,
        &expect![[r#"
            Some(1): pumpkin: the cached release with SHA-256 <checksum> names commit 1234567890123456789012345678901234567890, not 1234567890123456789012345678901234567891: compare .pumpkinversion with file://<scratch>/releases/download/v1/pumpkinversion
            workspace files: (none)
            cache entries: <checksum> locks"#]],
    );
}

#[test]
#[ignore = "runs the installed pumpkin in an empty image: run cargo xtask live docker"]
fn build_root() {
    let held = TestDir::new();
    let root = held.path();
    let built = process::command(env!("CARGO_BIN_EXE_xtask"), Environment::Inherited)
        .arg("build")
        .arg("--root")
        .arg(root)
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let printed = String::from_utf8(built.stdout).unwrap();
    let printed = printed.replace(&root.display().to_string(), "<root>");
    expect![[r#"
        <root>/bin/pumpkin
        <root>/bin/pumpkin-admin
        <root>/bin/buildifier
        <root>/bin/ec2-boot
        <root>/bin/sandboxd
        <root>/bin/pumpkin-modal.py
        <root>/bin/pumpkin-modal.py.lock
        "#]]
    .assert_eq(&printed);
    fs::write(
        root.join("Dockerfile"),
        "FROM scratch\nCOPY bin/pumpkin /pumpkin\nRUN [\"/pumpkin\", \"--version\"]\n\
         COPY bin/pumpkin-admin /pumpkin-admin\nRUN [\"/pumpkin-admin\", \"--version\"]\n",
    )
    .unwrap();
    let built = test_support::docker()
        .args(["build", "--output=type=cacheonly"])
        .arg(root)
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
}
