//! What the tests that need Docker share: the engine they run on, and the image their
//! sandboxes run. Those tests are ignored.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::OnceLock;

use docker::{ContextName, Engine};
use schema::ir::Image;
use schema::provider::SandboxId;
use test_support::{BUSYBOX, docker, sandboxd};

use crate::provider::{Locals, Subject};

/// The engine the tests' context reaches ([`test_support::docker_context`]).
pub(crate) fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let name = ContextName::try_from(test_support::docker_context()).unwrap();
        Engine::connect(name).unwrap()
    })
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
        test_support::build(&dockerfile, "pumpkin-tests:workspace")
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
