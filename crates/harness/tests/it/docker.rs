//! The recipe that adds Codex to an image, built by a real Docker engine onto a pinned `busybox`:
//! where the image holds Codex, the modes of its files, and that Codex runs. The build fetches
//! the pinned tarball and the base, and needs Docker, so the test is ignored, as the Docker
//! provider's are: `cargo xtask ci --docker` runs it, through the context
//! `PUMPKIN_DOCKER_CONTEXT` names, `default` if it is unset.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]
#![expect(
    clippy::print_stderr,
    reason = "a test that can't remove its image says so"
)]

use std::io::Write as _;
use std::process::{Command, Stdio};

use expect_test::{Expect, expect};
use harness::codex::CODEX_0_167_0;
use stdx::process::Environment;

/// A small public image, pinned by digest.
// Repeated in `providers/docker/tests/it.rs`, `crates/runtime/tests/it/docker.rs` and
// `crates/pumpkin/tests/it.rs`: change all four.
const BUSYBOX: &str =
    "busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e";

/// The label that marks what the test makes in the engine.
const LABEL: &str = "pumpkin.test=harness";

/// `docker`, run as the provider runs it, against the tests' context alone.
// Repeated in `providers/docker/tests/it.rs` and `crates/runtime/tests/it/docker.rs`: change
// all three.
fn docker() -> Command {
    // Only what the provider's `docker` keeps of the environment, for the reasons it gives.
    let kept = Environment::Only(&["HOME", "PATH", "DOCKER_CONFIG"]);
    let mut docker = stdx::process::command("docker", kept);
    let context = std::env::var("PUMPKIN_DOCKER_CONTEXT").unwrap_or_else(|_| "default".to_owned());
    docker.arg(format!("--context={context}"));
    docker
}

/// An image tag that is removed when it is dropped, and so the image, which no other tag names:
/// the tag holds this process's ID, and the image's files the pin's, so that no other process
/// holds the same image under a tag of its own.
struct Tagged(String);

impl Drop for Tagged {
    fn drop(&mut self) {
        let removed = docker().args(["image", "rm", &self.0]).output();
        if !removed.is_ok_and(|removed| removed.status.success()) {
            eprintln!("the test's image tag {} can't be removed", self.0);
        }
    }
}

/// Builds `dockerfile` into an image tagged `tag`, labeled [`LABEL`].
fn build(dockerfile: &str, tag: &str) -> Tagged {
    // Without provenance, which would add an attestation manifest to the image.
    let mut build = docker()
        .args(["build", "--quiet", "--provenance=false", "--label", LABEL])
        .args(["--tag", tag, "-"])
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
    Tagged(tag.to_owned())
}

/// Builds Codex's recipe onto `BUSYBOX`, runs `script` in a container of the image, and
/// compares with `expect` what it prints.
fn check(script: &str, expect: &Expect) {
    let tag = format!("pumpkin-tests/harness-{}:codex", std::process::id());
    let image = build(&CODEX_0_167_0.recipe(BUSYBOX), &tag);
    // With no network, which Codex's files need none of.
    let run = ["run", "--rm", "--network=none", "--label", LABEL];
    let mut docker = docker();
    let output = docker.args(run).arg(&image.0).args(["sh", "-c", script]);
    let output = output.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    expect.assert_eq(String::from_utf8(output.stdout).unwrap().trim_end());
}

/// Codex's files in an image built on `busybox`: each one's mode, owner and path below the
/// reserved directory, and what Codex prints as it runs.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn codex_layer() {
    let script = "cd /harness/codex-0.167.0 && find . | sort | xargs stat -c '%A %u:%g %n' \
                  && ./bin/codex --version";
    check(
        script,
        &expect![[r#"
        dr-xr-xr-x 0:0 .
        dr-xr-xr-x 0:0 ./bin
        -r-xr-xr-x 0:0 ./bin/codex
        -r-xr-xr-x 0:0 ./bin/codex-code-mode-host
        -r--r--r-- 0:0 ./codex-package.json
        dr-xr-xr-x 0:0 ./codex-path
        -r-xr-xr-x 0:0 ./codex-path/rg
        codex-cli 0.167.0"#]],
    );
}
