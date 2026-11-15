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

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn claude_code_layer() {
    // The libraries the loader finds for `claude`, without their tabs and their addresses, which
    // differ from run to run; `claude` run as the library's `argv` runs it; and `claude` run again
    // under the names its shell's `find` and `grep` give it.
    let script = "cd /harness/claude-code-2.1.296 && find . | sort | xargs stat -c '%A %u:%g %n' \
                  && ./lib/ld-linux-x86-64.so.2 --list ./claude \
                  | tr -d '\t' | sed 's/ (0x[0-9a-f]*)//' \
                  && /harness/claude-code-2.1.296/claude --version \
                  && (exec -a bfs /harness/claude-code-2.1.296/claude --version) \
                  && (exec -a ugrep /harness/claude-code-2.1.296/claude --version)";
    check(
        "harness/claude_code/layer/Dockerfile",
        script,
        &expect![[r#"
            dr-xr-xr-x 0:0 .
            -r--r--r-- 0:0 ./LICENSE.md
            -r-xr-xr-x 0:0 ./claude
            dr-xr-xr-x 0:0 ./lib
            -r-xr-xr-x 0:0 ./lib/ld-linux-x86-64.so.2
            -r--r--r-- 0:0 ./lib/libc.so.6
            -r--r--r-- 0:0 ./lib/libdl.so.2
            -r--r--r-- 0:0 ./lib/libm.so.6
            -r--r--r-- 0:0 ./lib/libpthread.so.0
            -r--r--r-- 0:0 ./lib/librt.so.1
            linux-vdso.so.1
            librt.so.1 => /harness/claude-code-2.1.296/lib/librt.so.1
            libc.so.6 => /harness/claude-code-2.1.296/lib/libc.so.6
            /harness/claude-code-2.1.296/lib/ld-linux-x86-64.so.2 => ./lib/ld-linux-x86-64.so.2
            libpthread.so.0 => /harness/claude-code-2.1.296/lib/libpthread.so.0
            libdl.so.2 => /harness/claude-code-2.1.296/lib/libdl.so.2
            libm.so.6 => /harness/claude-code-2.1.296/lib/libm.so.6
            2.1.296 (Claude Code)
            bfs 4.1.1

            Copyright © Tavian Barnes and the bfs contributors
            No rights reserved (https://opensource.org/license/0BSD)

            CONFFLAGS := --enable-release --with-oniguruma --without-libselinux --without-libacl
            CC        := /usr/lib/llvm-23/bin/clang --target=x86_64-linux-gnu --sysroot=/opt/linux-sysroot-glibc
            CPPFLAGS  := -Isrc -Igen -include src/prelude.h -DNDEBUG -MD -MP -Wundef-prefix=BFS_ -Dposix_getdents=__bfs_poison_posix_getdents_musl_1_2_5 -Dgetdents64=__bfs_poison_getdents64_glibc_2_30 -Dposix_spawn_file_actions_addfchdir_np=__bfs_poison_addfchdir_np_glibc_2_29 -I/tmpfs/bun/embedded/oniguruma/src
            CFLAGS    := -O3 -std=c23 -pthread -Wformat=2 -Wimplicit-fallthrough -Werror=implicit -Wmissing-declarations -Wmissing-variable-declarations -Wshadow -Wsign-compare -Wstrict-prototypes -g -Wall 
            LDFLAGS   := -Wl,-z,now -L/tmpfs/bun/build/release/deps/oniguruma -no-pie
            LDLIBS    := -lonig -lrt

            https://tavianator.com/projects/bfs.html
            ugrep 7.8.4 x86_64-pc-linux-gnu +sse2; -P:pcre2jit; -z:zlib,bzip2,zstd,brotli,7z,tar/pax/cpio/zip
            License: BSD-3-Clause; ugrep user manual: <https://ugrep.com>
            Written by Robert van Engelen and others: <https://github.com/Genivia/ugrep>
            Ugrep utilizes the RE/flex regex library: <https://github.com/Genivia/RE-flex>
            the layer on another base: the same"#]],
    );
}
