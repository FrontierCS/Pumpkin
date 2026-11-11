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

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use docker::{ContextName, Engine};
use schema::ir::Image;
use schema::provider::SandboxId;
use stdx::process::Environment;

use crate::provider::{Locals, Subject};

/// A small public image, pinned by digest.
// Repeated in `providers/docker/tests/it.rs`, `crates/pumpkin/tests/it/main.rs` and
// `crates/harness/tests/it/docker.rs`: change all four.
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

/// The engine the tests' context reaches: `PUMPKIN_DOCKER_CONTEXT`'s, or `default`'s.
pub(crate) fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let name = std::env::var("PUMPKIN_DOCKER_CONTEXT").unwrap_or_else(|_| "default".to_owned());
        Engine::connect(ContextName::try_from(name).unwrap()).unwrap()
    })
}

/// `docker`, run as the provider runs it, against the tests' context alone.
// Repeated in `providers/docker/tests/it.rs` and
// `crates/harness/tests/it/docker.rs`: change all three.
fn docker() -> Command {
    // Only what the provider's `docker` keeps of the environment, for the reasons it gives.
    let kept = Environment::Only(&["HOME", "PATH", "DOCKER_CONFIG"]);
    let mut docker = stdx::process::command("docker", kept);
    docker.arg(format!("--context={}", engine().report().context.as_str()));
    docker
}

/// The ID of the image the tests share, which the IR accepts as a pinned image: `busybox` with
/// a world-writable `/workspace`, which a role's user needs, since the writable layer is the
/// workspace. Built once, from the pinned image, and tagged, so that no build leaves an image
/// behind untagged; a build from the cache gives the same ID as the last, so test processes
/// that run at once share the image.
// Built alike in `providers/docker/tests/it.rs`: change both.
pub(crate) fn image() -> Image {
    static ID: OnceLock<String> = OnceLock::new();
    let id = ID.get_or_init(|| {
        let dockerfile = format!("FROM {BUSYBOX}\nRUN mkdir --mode=1777 /workspace\n");
        // Without provenance, for the reason `image()` in the Docker provider's tests gives
        // (`providers/docker/tests/it.rs`).
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

impl Subject for Engine {
    fn locals(&self, dir: &Path) -> Locals {
        // A `sandboxd` that fails as it starts, saying why.
        let mute = dir.join("stand-in-sandboxd");
        let script = "#!/bin/sh\necho 'sandboxd: a stand-in, which exits without dialing' >&2\n";
        fs::write(&mute, script).unwrap();
        fs::set_permissions(&mute, fs::Permissions::from_mode(0o755)).unwrap();
        Locals {
            image: image(),
            sandboxd: sandboxd(),
            mute,
        }
    }

    // Pausing freezes every process in the container, `sandboxd` among them, before it returns.
    fn stall(&self, id: &SandboxId) {
        let paused = docker().args(["pause", id.as_str()]).output().unwrap();
        assert!(paused.status.success(), "{paused:?}");
    }
}
