//! The release launcher against pinned archives read through curl, and the static install.

use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::Path;
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use expect_test::{Expect, expect};
use test_support::TestDir;
use xshell::Shell;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/pumpkin");
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
    let scratch = held.path();
    let home = scratch.join("home");
    let sh = Shell::new().unwrap();
    let checksum = match release {
        Release::None => WRONG_SHA.to_owned(),
        Release::WrongCommit => archive(&sh, scratch, WRONG_COMMIT),
        Release::Current | Release::Cached | Release::Concurrent => archive(&sh, scratch, COMMIT),
    };
    let (cache, xdg) = if matches!(release, Release::Cached) {
        let root = scratch.join("cache");
        (root.join("pumpkin"), Some(root))
    } else {
        (home.join(".cache/pumpkin"), None)
    };
    let probes = matches!(release, Release::Concurrent).then(|| {
        let path = env::var_os("PATH").unwrap();
        let bin = scratch.join("bin");
        fs::create_dir(&bin).unwrap();
        let tools = ["flock", "curl"].map(|tool| {
            let original = env::split_paths(&path)
                .map(|dir| dir.join(tool))
                .find(|file| file.is_file())
                .unwrap();
            let probe = bin.join(tool);
            fs::write(
                &probe,
                format!(
                    r#"#!/bin/sh
printf '.\n' >> "$PROBES/{tool}"
exec "$REAL_{name}" "$@"
"#,
                    name = tool.to_ascii_uppercase()
                ),
            )
            .unwrap();
            fs::set_permissions(probe, fs::Permissions::from_mode(0o755)).unwrap();
            original
        });
        let path = env::join_paths((std::iter::once(bin)).chain(env::split_paths(&path))).unwrap();
        (tools, path)
    });
    let run = |name: &str, pin: Option<&str>| {
        let repo = scratch.join(name);
        let subdir = repo.join("subdir");
        fs::create_dir_all(&subdir).unwrap();
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
        fs::write(repo.join("pumpkin"), launcher).unwrap();
        if let Some(pin) = pin {
            fs::write(
                repo.join(".pumpkinversion"),
                pin.replace("<checksum>", &checksum),
            )
            .unwrap();
        }
        let path = if lacking {
            let path = env::var_os("PATH").unwrap();
            let bin = scratch.join("bin");
            fs::create_dir(&bin).unwrap();
            for tool in ["sh", "uname", "dirname"] {
                let found = env::split_paths(&path)
                    .map(|dir| dir.join(tool))
                    .find(|file| file.is_file())
                    .unwrap();
                symlink(found, bin.join(tool)).unwrap();
            }
            bin.into_os_string()
        } else if let Some((_, path)) = &probes {
            path.clone()
        } else {
            env::var_os("PATH").unwrap()
        };
        let sh = Shell::new().unwrap();
        sh.change_dir(subdir);
        let call = sh
            .cmd("sh")
            .args(["../pumpkin", "check", "task with spaces.star"])
            .env("HOME", &home)
            .env("PATH", path);
        let call = if let Some(root) = &xdg {
            call.env("XDG_CACHE_HOME", root)
        } else {
            call.env_remove("XDG_CACHE_HOME")
        };
        let call = if let Some(([flock, curl], _)) = &probes {
            call.env("PROBES", scratch)
                .env("REAL_FLOCK", flock)
                .env("REAL_CURL", curl)
        } else {
            call
        };
        call.ignore_status().quiet().output().unwrap()
    };
    if matches!(release, Release::Cached) {
        let pin = format!("v1 {COMMIT} {checksum}");
        let first = run("first", Some(&pin));
        assert!(
            first.status.success(),
            "{}",
            String::from_utf8_lossy(&first.stderr)
        );
        assert_eq!(listed(&scratch.join("first/.pumpkin")), ".gitignore");
        fs::remove_dir_all(scratch.join("releases")).unwrap();
    }
    let output = if matches!(release, Release::Concurrent) {
        let locks = cache.join("locks");
        fs::create_dir_all(&locks).unwrap();
        let gate = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(locks.join(&checksum))
            .unwrap();
        gate.lock().unwrap();
        thread::scope(|scope| {
            let first = scope.spawn(|| run("first", pin));
            let second = scope.spawn(|| run("repo", pin));
            let start = Instant::now();
            let attempted = || {
                fs::read_to_string(scratch.join("flock"))
                    .is_ok_and(|text| text.lines().count() == 2)
            };
            while !attempted() && start.elapsed() < Duration::from_secs(2) {
                thread::sleep(Duration::from_millis(5));
            }
            let both_cold = attempted();
            gate.unlock().unwrap();
            let first = first.join().unwrap();
            let mut second = second.join().unwrap();
            assert!(both_cold, "both launchers must reach the cold cache lock");
            for output in [&first, &second] {
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            assert_eq!(listed(&scratch.join("first/.pumpkin")), ".gitignore");
            let downloads = fs::read_to_string(scratch.join("curl"))
                .unwrap()
                .lines()
                .count();
            second.stderr = format!("downloads: {downloads}\n").into_bytes();
            second
        })
    } else {
        run("repo", pin)
    };
    let workspace = scratch.join("repo/.pumpkin");
    let kept = listed(&cache);
    let entry = cache.join(&checksum);
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
        listed(&workspace),
        kept,
    );
    let report = report.replace(&scratch.display().to_string(), "<scratch>");
    let report = match release {
        Release::None => report,
        _ => report.replace(&checksum, "<checksum>"),
    };
    expect.assert_eq(&report);
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

fn archive(sh: &Shell, scratch: &Path, commit: &str) -> String {
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
    sh.cmd("tar")
        .args(["--create", "--xz", "--file"])
        .arg(&file)
        .arg("--directory")
        .arg(tree)
        .args(["bin", "commit"])
        .run()
        .unwrap();
    let output = sh.cmd("sha256sum").arg(file).read().unwrap();
    output.split_whitespace().next().unwrap().to_owned()
}

#[test]
fn missing_pin() {
    check(
        Release::None,
        None,
        false,
        &expect![[r#"
            Some(1): pumpkin: there is no .pumpkinversion beside this script: write the release tag, its full commit and the 64-digit SHA-256 from the .sha256 at https://github.com/FrontierCS/Pumpkin/releases
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
            Some(1): pumpkin: .pumpkinversion has no release checksum: write the release tag, its full commit and the 64-digit SHA-256 from the .sha256 at https://github.com/FrontierCS/Pumpkin/releases
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
            pumpkin: file://<scratch>/releases/download/v1/pumpkin-v1-x86_64-unknown-linux-musl.tar.xz has the SHA-256 checksum <checksum>, not 0000000000000000000000000000000000000000000000000000000000000000, which .pumpkinversion pins: check the release and its published file://<scratch>/releases/download/v1/pumpkin-v1-x86_64-unknown-linux-musl.tar.xz.sha256
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
            pumpkin: the release at file://<scratch>/releases/download/v1/pumpkin-v1-x86_64-unknown-linux-musl.tar.xz names commit 1234567890123456789012345678901234567891, not 1234567890123456789012345678901234567890: fix the commit or checksum in .pumpkinversion
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
            Some(1): pumpkin: the cached release with SHA-256 <checksum> names commit 1234567890123456789012345678901234567890, not 1234567890123456789012345678901234567891: fix the commit or checksum in .pumpkinversion
            workspace files: (none)
            cache entries: <checksum> locks"#]],
    );
}

#[test]
#[ignore = "runs the installed pumpkin in an empty image: run cargo xtask live docker"]
fn install_root() {
    let held = TestDir::new();
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
    fs::write(
        root.join("Dockerfile"),
        "FROM scratch\nCOPY bin/pumpkin /pumpkin\nRUN [\"/pumpkin\", \"--version\"]\n",
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
