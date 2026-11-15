//! The engine's publish slots, without Docker: a burst of looks at images' platforms, more than
//! there are slots, runs at most as many at once as there are slots, the rest waiting, which the
//! engine says once.

use std::io::{BufRead as _, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;
use std::{env, fs, thread};

use engine::{ContextName, Engine, Platform};
use expect_test::expect;
use schema::ir::Image;
use stdx::process::Environment;

/// The variable that names the child's directory, which holds the stand-in, in a child that
/// [`publishes_at_once`] runs, whose `PATH` finds the stand-in first.
const CHILD: &str = "PUMPKIN_DOCKER_PUBLISH_STAND_IN";

/// How many looks the burst makes, twice the engine's slots.
const LOOKS: usize = 8;

/// What the engine says the first time a look waits for a slot, up to the holders it names, of
/// which one that has yet to write its name is named by its slot's.
const WAITING: &str = "pumpkin: waiting for a publish slot: all 4 held, by ";

/// The stand-in `docker`: `image inspect` of an image's platform waits until the slots' count,
/// 4, have run at once and the test has written `go`, or a minute passes, notes how many run,
/// then answers as an engine of a manifest for `linux/amd64` does; the three commands with which
/// the engine is reached answer as Docker's would; any other fails.
fn stand_in(dir: &Path) -> String {
    let dir = dir.display();
    format!(
        r#"#!/bin/sh
case " $* " in
*" image inspect "*)
    mkdir "{dir}/running.$$"
    i=0
    while {{ [ ! -e "{dir}/full" ] || [ ! -e "{dir}/go" ]; }} && [ $i -lt 1200 ]; do
        [ "$(ls --directory "{dir}"/running.* | wc --lines)" -lt 4 ] || : > "{dir}/full"
        sleep 0.05
        i=$((i+1))
    done
    ls --directory "{dir}"/running.* | wc --lines >> "{dir}/at-once"
    rmdir "{dir}/running.$$"
    echo '{{"Os":"linux","Architecture":"amd64","Descriptor":{{"mediaType":"application/vnd.oci.image.manifest.v1+json"}}}}' ;;
*" context inspect "*) echo unix:///nonexistent/docker-stand-in.sock ;;
*" info --format "*)
    echo '{{"MemoryLimit": true, "SwapLimit": true, "CpuCfsQuota": true, "CPUSet": true, "PidsLimit": true, "MemTotal": 8589934592, "ID": "stand-in", "DriverStatus": [["driver-type", "io.containerd.snapshotter.v1"]], "SecurityOptions": ["name=seccomp,profile=builtin"], "KernelVersion": "7.0.0", "OperatingSystem": "stand-in", "ServerVersion": "29.9.2", "ClientInfo": {{"Version": "29.9.2"}}}}' ;;
*" version --format "*) echo linux/amd64 ;;
*) echo "the stand-in docker doesn't know \`$*\`" >&2; exit 1 ;;
esac
"#
    )
}

// The first looks hold their slots until the engine has said that it waits, so that the others
// find every slot held.
#[test]
fn publishes_at_once() {
    if let Some(dir) = env::var_os(CHILD) {
        return child(Path::new(&dir));
    }
    let scratch = tempfile::Builder::new()
        .prefix("publish-")
        .tempdir()
        .unwrap();
    let dir = scratch.path();
    fs::create_dir_all(dir.join("bin")).unwrap();
    let docker = dir.join("bin/docker");
    fs::write(&docker, stand_in(dir)).unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();
    let path = env::var_os("PATH").unwrap_or_default();
    let path = env::join_paths([dir.join("bin")].into_iter().chain(env::split_paths(&path)));
    let mut child = stdx::process::command(env::current_exe().unwrap(), Environment::Inherited);
    child.args(["--exact", "publish::publishes_at_once", "--include-ignored"]);
    let child = child.env(CHILD, dir).env("PATH", path.unwrap());
    let mut child = child.stderr(Stdio::piped()).spawn().unwrap();
    let mut stderr = String::new();
    for line in BufReader::new(child.stderr.take().unwrap()).lines() {
        let line = line.unwrap();
        if line.starts_with(WAITING) {
            fs::write(dir.join("go"), "").unwrap();
        }
        stderr.push_str(&line);
        stderr.push('\n');
    }
    let status = child.wait().unwrap();
    assert!(status.success(), "the child failed: {stderr}");
    let at_once: Vec<usize> = (fs::read_to_string(dir.join("at-once")).unwrap().lines())
        .map(|count| count.trim().parse().unwrap())
        .collect();
    let looked = fs::read_to_string(dir.join("looked")).unwrap();
    let said = stderr.matches(WAITING);
    let lines = [
        format!("looks: {}", at_once.len()),
        format!("most at once: {}", at_once.iter().max().unwrap()),
        format!("each found the platform: {looked}"),
        format!("said it waits for a slot, times: {}", said.count()),
    ];
    expect![[r#"
        looks: 8
        most at once: 4
        each found the platform: true
        said it waits for a slot, times: 1"#]]
    .assert_eq(&lines.join("\n"));
}

/// As the child: connects to the stand-in's engine, with its directory in `dir`, then looks at
/// an image's platform from [`LOOKS`] threads at once, and writes in `looked` whether each found
/// `linux/amd64`.
fn child(dir: &Path) {
    let context = ContextName::try_from("stand-in".to_owned()).unwrap();
    let engine = Engine::connect(context, &dir.join(".pumpkin/docker")).unwrap();
    let image = Image::try_from(format!("sha256:{}", "a".repeat(64))).unwrap();
    let platform = Platform::sandboxd();
    let found: Vec<bool> = thread::scope(|scope| {
        let looks: Vec<_> = (0..LOOKS)
            .map(|_| scope.spawn(|| engine.check_platform(&image, &platform, "the test runs")))
            .collect();
        let found = looks.into_iter().map(|look| look.join().unwrap());
        found.map(|checked| checked.is_ok()).collect()
    });
    fs::write(
        dir.join("looked"),
        found.iter().all(|found| *found).to_string(),
    )
    .unwrap();
}
