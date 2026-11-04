//! What the tests that need Docker share: the static `sandboxd` they run with, the engine they
//! run on, and the image their sandboxes run. Those tests are ignored, as the Docker
//! provider's are, and fail without `PUMPKIN_SANDBOXD`; `cargo xtask ci --docker` sets it and
//! runs them. They reach Docker through the context `PUMPKIN_DOCKER_CONTEXT` names, `default`
//! if it is unset.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use pumpkin_docker::{ContextName, Engine, RunTag};
use pumpkin_schema::ir::Image;

/// A small public image, pinned by digest.
const BUSYBOX: &str =
    "busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e";

/// The static `sandboxd` that `PUMPKIN_SANDBOXD` names.
pub(crate) fn sandboxd() -> PathBuf {
    let Some(sandboxd) = std::env::var_os("PUMPKIN_SANDBOXD") else {
        panic!(
            "`PUMPKIN_SANDBOXD` is unset: run the Docker tests with `cargo xtask ci --docker`, \
             which builds sandboxd and sets it"
        );
    };
    PathBuf::from(sandboxd).canonicalize().unwrap()
}

/// The context the tests reach Docker through: `PUMPKIN_DOCKER_CONTEXT`, or `default`.
fn context() -> ContextName {
    let name = std::env::var("PUMPKIN_DOCKER_CONTEXT").unwrap_or_else(|_| "default".to_owned());
    ContextName::try_from(name).unwrap()
}

/// The engine the tests' context reaches.
pub(crate) fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| Engine::connect(context()).unwrap())
}

/// `docker`, run as the provider runs it, against the tests' context alone.
fn docker() -> Command {
    // Only what the provider's `docker` keeps of the environment, for the reasons it gives.
    let kept = ["HOME", "PATH", "DOCKER_CONFIG"]
        .into_iter()
        .filter_map(|name| Some((name, std::env::var_os(name)?)));
    let mut docker = Command::new("docker");
    docker.env_clear().envs(kept);
    docker.arg(format!("--context={}", context().as_str()));
    docker
}

/// The IDs of the run's containers, running or not, as `docker ps` finds them by their run's
/// label.
pub(crate) fn containers(run: &RunTag) -> Vec<String> {
    let filter = format!("label=pumpkin.run={}", run.as_str());
    let ps = ["ps", "--all", "--quiet", "--no-trunc", "--filter", &filter];
    let output = docker().args(ps).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let ids = String::from_utf8(output.stdout).unwrap();
    ids.lines().map(str::to_owned).collect()
}

/// The ID of the image the tests share, which the IR accepts as a pinned image: `busybox` with
/// a world-writable `/workspace`, which a role's user needs, since the writable layer is the
/// workspace. Built once, from the pinned image, and tagged, so that no build leaves an image
/// behind untagged; a build from the cache gives the same ID as the last, so test processes
/// that run at once share the image.
pub(crate) fn image() -> Image {
    static ID: OnceLock<String> = OnceLock::new();
    let id = ID.get_or_init(|| {
        let dockerfile = format!("FROM {BUSYBOX}\nRUN mkdir --mode=1777 /workspace\n");
        // A provenance attestation records each build's own ID and times, so a build with one
        // gets an ID of its own; and as a build moves the tag to its ID, the containerd image
        // store keeps no image by the ID the tag had, which another test process, running at
        // once, may be using.
        let mut build = docker()
            .args(["build", "--quiet", "--provenance=false"])
            .args(["--tag=pumpkin-tests:workspace", "-"])
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
    });
    Image::try_from(id.clone()).unwrap()
}
