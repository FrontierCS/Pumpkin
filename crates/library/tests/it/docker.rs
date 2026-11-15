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

/// Builds `dockerfile` into an image tagged `tag`, given `contexts`, each a named context's name
/// and an image reference.
fn build(dockerfile: &str, contexts: &[(&str, &str)], tag: &str) -> Tagged {
    test_support::build(dockerfile, contexts, tag);
    Tagged(tag.to_owned())
}

/// The diff IDs of the layers of the image tagged `image`.
fn layers(image: &Tagged) -> Vec<String> {
    let inspect = [
        "image",
        "inspect",
        "--format",
        "{{json .RootFS.Layers}}",
        &image.0,
    ];
    let output = docker().args(inspect).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Builds the library's `recipe` onto `BUSYBOX`, given each of `named`, a named context's name
/// and the library's recipe of its image, runs `script` in a container of the image, and
/// compares with `expect` what it prints, and whether the recipe adds the same layers onto
/// another base.
fn check(recipe: &str, named: &[(&str, &str)], script: &str, expect: &Expect) {
    let recipe = file(recipe);
    let tag = |name: &str| format!("pumpkin-tests/library-{}:{name}", std::process::id());
    let named: Vec<(&str, Tagged)> = (named.iter())
        .map(|(name, recipe)| (*name, build(&file(recipe), &[], &tag(name))))
        .collect();
    // The build of `recipe` onto `base`, an image's tag, tagged `name`.
    let layered = |base: &str, name: &str| {
        let mut contexts = vec![("base", base)];
        contexts.extend(named.iter().map(|(name, image)| (*name, image.0.as_str())));
        build(&recipe, &contexts, &tag(name))
    };
    let image = layered(BUSYBOX, "layer");
    // With no network, which a harness's files need none of.
    let run = ["run", "--rm", "--network=none", "--label", LABEL];
    let mut docker = docker();
    let output = docker.args(run).arg(&image.0).args(["sh", "-c", script]);
    let output = output.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let other = build(
        "FROM base\nRUN mkdir /other\n",
        &[("base", BUSYBOX)],
        &tag("other"),
    );
    let on_other = layered(&other.0, "layer-on-other");
    // `other` is `BUSYBOX` and one layer more: the layers the recipe adds are those past each
    // base's.
    let busybox_count = layers(&other).len() - 1;
    let same = if layers(&image)[busybox_count..] == layers(&on_other)[busybox_count + 1..] {
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
        &[],
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
        &[],
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

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn zcode_layer() {
    // The libraries the loader finds for `node`, without their tabs and their addresses, which
    // differ from run to run; `node` started as itself, and starting itself again as a child, as
    // a harness's worker starts; and ZCode run as the library's `argv` runs it.
    let script = "cd /harness && find . | sort | xargs stat -c '%A %u:%g %n' \
                  && ./node-24.14.0/lib/ld-linux-x86-64.so.2 --list ./node-24.14.0/node \
                  | tr -d '\t' | sed 's/ (0x[0-9a-f]*)//' \
                  && /harness/node-24.14.0/node --version \
                  && /harness/node-24.14.0/node -e 'process.stdout.write(require(\"node:child_process\")\
                  .execFileSync(process.execPath, [\"-p\", \"`${process.execPath} ${process.version}`\"]))' \
                  && /harness/node-24.14.0/node /harness/zcode-0.16.9/zcode.cjs --version";
    check(
        "harness/zcode/layer/Dockerfile",
        &[("node", "harness/node/layer/Dockerfile")],
        script,
        &expect![[r#"
            dr-xr-xr-x 0:0 .
            dr-xr-xr-x 0:0 ./node-24.14.0
            -r--r--r-- 0:0 ./node-24.14.0/LICENSE
            dr-xr-xr-x 0:0 ./node-24.14.0/lib
            -r-xr-xr-x 0:0 ./node-24.14.0/lib/ld-linux-x86-64.so.2
            -r--r--r-- 0:0 ./node-24.14.0/lib/libc.so.6
            -r--r--r-- 0:0 ./node-24.14.0/lib/libdl.so.2
            -r--r--r-- 0:0 ./node-24.14.0/lib/libgcc_s.so.1
            -r--r--r-- 0:0 ./node-24.14.0/lib/libm.so.6
            -r--r--r-- 0:0 ./node-24.14.0/lib/libpthread.so.0
            -r--r--r-- 0:0 ./node-24.14.0/lib/libstdc++.so.6
            -r-xr-xr-x 0:0 ./node-24.14.0/node
            dr-xr-xr-x 0:0 ./zcode-0.16.9
            -r--r--r-- 0:0 ./zcode-0.16.9/LICENSE
            -r--r--r-- 0:0 ./zcode-0.16.9/THIRD-PARTY-NOTICES.md
            dr-xr-xr-x 0:0 ./zcode-0.16.9/provider
            -r--r--r-- 0:0 ./zcode-0.16.9/provider/zcode-builtin.json
            -r-xr-xr-x 0:0 ./zcode-0.16.9/zcode.cjs
            linux-vdso.so.1
            libdl.so.2 => /harness/node-24.14.0/lib/libdl.so.2
            libstdc++.so.6 => /harness/node-24.14.0/lib/libstdc++.so.6
            libm.so.6 => /harness/node-24.14.0/lib/libm.so.6
            libgcc_s.so.1 => /harness/node-24.14.0/lib/libgcc_s.so.1
            libpthread.so.0 => /harness/node-24.14.0/lib/libpthread.so.0
            libc.so.6 => /harness/node-24.14.0/lib/libc.so.6
            /harness/node-24.14.0/lib/ld-linux-x86-64.so.2 => ./node-24.14.0/lib/ld-linux-x86-64.so.2
            v24.14.0
            /harness/node-24.14.0/node v24.14.0
            0.16.9
            the layer on another base: the same"#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn qwen_code_layer() {
    // The package's thousand files, by the count of those not root's or writable; `node` started
    // as itself, and starting itself again as a child, as a harness's worker starts; Qwen Code run
    // as the library's `argv` runs it; and the ripgrep it searches with.
    let script = "cd /harness && stat -c '%A %u:%g %n' qwen-code-0.25.0 \
                  qwen-code-0.25.0/cli-entry.js qwen-code-0.25.0/vendor/ripgrep/x64-linux/rg \
                  && find . '(' ! -user 0 -o ! -group 0 -o -perm /0222 ')' | wc -l \
                  && /harness/node-24.14.0/node --version \
                  && /harness/node-24.14.0/node -e 'process.stdout.write(require(\"node:child_process\")\
                  .execFileSync(process.execPath, [\"-p\", \"`${process.execPath} ${process.version}`\"]))' \
                  && /harness/node-24.14.0/node /harness/qwen-code-0.25.0/cli-entry.js --version \
                  && /harness/qwen-code-0.25.0/vendor/ripgrep/x64-linux/rg --version | head -n 1";
    check(
        "harness/qwen_code/layer/Dockerfile",
        &[("node", "harness/node/layer/Dockerfile")],
        script,
        &expect![[r#"
            dr-xr-xr-x 0:0 qwen-code-0.25.0
            -r-xr-xr-x 0:0 qwen-code-0.25.0/cli-entry.js
            -r-xr-xr-x 0:0 qwen-code-0.25.0/vendor/ripgrep/x64-linux/rg
            0
            v24.14.0
            /harness/node-24.14.0/node v24.14.0
            0.25.0
            ripgrep 15.0.0 (rev 3a612f88b8)
            the layer on another base: the same"#]],
    );
}
