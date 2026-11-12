//! What the tests of more than one of Pumpkin's crates share: a complete workflow
//! ([`WORKFLOW`]), a stand-in HTTP server's end of a connection ([`http`]), a directory's
//! listing ([`files`]), and what the tests that need Docker reach it with.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    reason = "a test helper fails the test that calls it"
)]

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use stdx::process::Environment;

pub mod http;

/// A complete workflow, in the IR's JSON.
///
/// Its roles are an agent with a model route, a train judge it may call, and a final judge that
/// scores its submission, in that order. Their images' digests, commands, files, standard input
/// and environment are placeholders.
pub const WORKFLOW: &str = include_str!("workflow.json");

/// The files and empty directories below `dir`, by their paths from it, sorted.
pub fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    let mut lines = Vec::new();
    for name in names {
        let path = dir.join(&name);
        if path.is_dir() {
            let below = files(&path);
            if below.is_empty() {
                lines.push(format!("{name}/"));
            }
            lines.extend(below.iter().map(|below| format!("{name}/{below}")));
        } else {
            lines.push(name);
        }
    }
    lines
}

/// A small public image, pinned by the digest of its index, that the tests' images build on.
pub const BUSYBOX: &str =
    "busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e";

/// The name of the Docker context the tests reach: `PUMPKIN_DOCKER_CONTEXT`, or `default` if it
/// is unset.
pub fn docker_context() -> String {
    std::env::var("PUMPKIN_DOCKER_CONTEXT").unwrap_or_else(|_| "default".to_owned())
}

/// `docker`, run as the Docker provider runs it, against [`docker_context`] alone.
pub fn docker() -> Command {
    let kept = Environment::Only(&["HOME", "PATH", "DOCKER_CONFIG"]);
    let mut docker = stdx::process::command("docker", kept);
    docker.arg(format!("--context={}", docker_context()));
    docker
}

/// Builds `dockerfile` with [`docker`] into an image tagged `tag`, and returns the image's ID.
pub fn build(dockerfile: &str, tag: &str) -> String {
    // Without provenance: an attestation gives each build an ID of its own, and as a build moves
    // its tag, the image store drops the image by the tag's old ID, which another test process
    // may be running.
    let mut build = docker()
        .args(["build", "--quiet", "--provenance=false", "--tag", tag, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Dropping its standard input once written ends the Dockerfile.
    (build.stdin.take().unwrap())
        .write_all(dockerfile.as_bytes())
        .unwrap();
    let build = build.wait_with_output().unwrap();
    assert!(build.status.success(), "{build:?}");
    String::from_utf8(build.stdout).unwrap().trim().to_owned()
}

/// The static `sandboxd` that `PUMPKIN_SANDBOXD` names, by its canonical path.
pub fn sandboxd() -> PathBuf {
    let Some(sandboxd) = std::env::var_os("PUMPKIN_SANDBOXD") else {
        panic!(
            "`PUMPKIN_SANDBOXD` is unset: run the Docker tests with `cargo xtask ci --docker`, \
             which builds sandboxd and sets it"
        );
    };
    PathBuf::from(sandboxd).canonicalize().unwrap()
}
