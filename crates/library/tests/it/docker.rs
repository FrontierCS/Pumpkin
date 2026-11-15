//! A harness's layer, built by a real Docker engine onto a pinned `busybox`, as the library's
//! `image()` builds it, with the agent's image as the named context `base`: where the image holds
//! the harness, the modes of its files, that the harness runs, and that the layer is the same on
//! another base. The build fetches the pinned files and the base, and needs Docker, so the tests
//! are ignored.

#![expect(
    clippy::print_stderr,
    reason = "a test that can't remove its image says so"
)]

use expect_test::{Expect, expect};
use test_support::{BUSYBOX, docker};

use super::file;

/// The label that marks the test's containers in the engine.
const LABEL: &str = "pumpkin.test=library";

/// An image tag that is removed when it is dropped, and so the image, which no other tag names:
/// the tag holds this process's ID, and the image's files the library's, so that no other
/// process holds the same image under a tag of its own.
struct Tagged(String);

impl Drop for Tagged {
    fn drop(&mut self) {
        let removed = docker().args(["image", "rm", &self.0]).output();
        if !removed.is_ok_and(|removed| removed.status.success()) {
            eprintln!("the test's image tag {} can't be removed", self.0);
        }
    }
}

/// Builds `dockerfile` into an image tagged `tag`, given `base`, an image reference, as the
/// named context `base`.
fn build(dockerfile: &str, base: &str, tag: &str) -> Tagged {
    test_support::build(dockerfile, &[("base", base)], tag);
    Tagged(tag.to_owned())
}

/// The diff ID of the last layer of the image tagged `image`.
fn last_layer(image: &Tagged) -> String {
    let inspect = [
        "image",
        "inspect",
        "--format",
        "{{json .RootFS.Layers}}",
        &image.0,
    ];
    let output = docker().args(inspect).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let layers: Vec<String> = serde_json::from_slice(&output.stdout).unwrap();
    layers.last().unwrap().clone()
}

/// Builds the library's `recipe` onto `BUSYBOX`, runs `script` in a container of the image, and
/// compares with `expect` what it prints, and whether the recipe adds the same layer onto
/// another base.
fn check(recipe: &str, script: &str, expect: &Expect) {
    let recipe = file(recipe);
    let tag = |name: &str| format!("pumpkin-tests/library-{}:{name}", std::process::id());
    let image = build(&recipe, BUSYBOX, &tag("layer"));
    // With no network, which a harness's files need none of.
    let run = ["run", "--rm", "--network=none", "--label", LABEL];
    let mut docker = docker();
    let output = docker.args(run).arg(&image.0).args(["sh", "-c", script]);
    let output = output.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let other = build("FROM base\nRUN mkdir /other\n", BUSYBOX, &tag("other"));
    let on_other = build(&recipe, &other.0, &tag("layer-on-other"));
    let same = if last_layer(&image) == last_layer(&on_other) {
        "the same"
    } else {
        "another"
    };
    let shown = String::from_utf8(output.stdout).unwrap();
    let shown = format!("{}\nthe layer on another base: {same}", shown.trim_end());
    expect.assert_eq(&shown);
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn codex_layer() {
    let script = "cd /harness/codex-0.167.0 && find . | sort | xargs stat -c '%A %u:%g %n' \
                  && ./bin/codex --version";
    check(
        "harness/codex/layer/Dockerfile",
        script,
        &expect![[r#"
            dr-xr-xr-x 0:0 .
            dr-xr-xr-x 0:0 ./bin
            -r-xr-xr-x 0:0 ./bin/codex
            -r-xr-xr-x 0:0 ./bin/codex-code-mode-host
            -r--r--r-- 0:0 ./codex-package.json
            dr-xr-xr-x 0:0 ./codex-path
            -r-xr-xr-x 0:0 ./codex-path/rg
            codex-cli 0.167.0
            the layer on another base: the same"#]],
    );
}
