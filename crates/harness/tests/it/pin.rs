//! A harness's pin and the recipe that adds the harness to an image: Codex's, which passes the
//! recipe check, and the pins and bases whose recipe can't be made.

use std::thread;

use expect_test::{Expect, expect};
use harness::codex::CODEX_0_167_0;
use harness::{Mode, Pin, PinnedFile};

fn check(actual: &str, expect: &Expect) {
    expect.assert_eq(actual);
}

#[test]
fn codex() {
    let base = "example/task/agent-base:0123";
    check(
        &CODEX_0_167_0.recipe(base),
        &expect![[r#"
            FROM busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e AS codex-0.167.0
            ADD --checksum=sha256:2dc9aa871abb513c3539d20d025f701faf1974c46ee906bda3a1e1f5026d4434 https://registry.npmjs.org/@openai/codex/-/codex-0.167.0-linux-x64.tgz /harness.tgz
            RUN tar -x -z -f /harness.tgz -C / package/vendor/x86_64-unknown-linux-musl/bin/codex package/vendor/x86_64-unknown-linux-musl/bin/codex-code-mode-host package/vendor/x86_64-unknown-linux-musl/codex-package.json package/vendor/x86_64-unknown-linux-musl/codex-path/rg
            FROM busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e AS codex-0.167.0-tree
            COPY --from=codex-0.167.0 /package/vendor/x86_64-unknown-linux-musl/ /out/harness/codex-0.167.0/
            RUN chmod 0444 /out/harness/codex-0.167.0/codex-package.json && chmod 0555 /out/harness /out/harness/codex-0.167.0 /out/harness/codex-0.167.0/bin /out/harness/codex-0.167.0/bin/codex /out/harness/codex-0.167.0/bin/codex-code-mode-host /out/harness/codex-0.167.0/codex-path /out/harness/codex-0.167.0/codex-path/rg
            FROM example/task/agent-base:0123
            COPY --link --from=codex-0.167.0-tree --chown=0:0 /out/ /
        "#]],
    );
}

#[test]
fn checked() {
    let base = "example/task/agent-base:0123";
    let text = CODEX_0_167_0.recipe(base);
    let checked = recipe::check("Dockerfile", text.as_bytes(), &[base]);
    check(&format!("{checked:?}"), &expect!["Ok(())"]);
}

#[test]
fn checks() {
    const CODEX: PinnedFile = file("bin/codex", Mode::Executable);
    const RG: PinnedFile = file("codex-path/rg", Mode::Executable);
    const GLOB: PinnedFile = file("bin/codex*", Mode::Executable);
    let pins = [
        Pin {
            name: "Codex-0.167.0",
            ..CODEX_0_167_0
        },
        Pin {
            url: "http://registry.npmjs.org/codex.tgz",
            ..CODEX_0_167_0
        },
        Pin {
            url: "https://registry.npmjs.org/$CODEX.tgz",
            ..CODEX_0_167_0
        },
        Pin {
            url: "https://registry.npmjs.org/codex.git",
            ..CODEX_0_167_0
        },
        Pin {
            sha256: "2dc9aa871abb513c3539d20d025f701faf1974c46ee906bda3a1e1f5026d4434",
            ..CODEX_0_167_0
        },
        Pin {
            dir: "package/../vendor",
            ..CODEX_0_167_0
        },
        Pin {
            files: &[GLOB],
            ..CODEX_0_167_0
        },
        Pin {
            files: &[],
            ..CODEX_0_167_0
        },
        Pin {
            files: &[RG, CODEX],
            ..CODEX_0_167_0
        },
        Pin {
            files: &[CODEX, CODEX],
            ..CODEX_0_167_0
        },
    ];
    let bases = [(CODEX_0_167_0, ""), (CODEX_0_167_0, "base\nRUN id")];
    let cases = (pins.into_iter().map(|pin| (pin, "base"))).chain(bases);
    let messages: Vec<String> = cases.map(|(pin, base)| panic_message(pin, base)).collect();
    check(
        &messages.join("\n"),
        &expect![[r#"
            internal error: entered unreachable code: pin Codex-0.167.0, written in code and its recipe made by the tests, with a base named as the engine accepted it, fails a check: the name `Codex-0.167.0` is not a harness's name: use a-z, 0-9, `.`, `_` and `-`, starting with a letter
            internal error: entered unreachable code: pin codex-0.167.0, written in code and its recipe made by the tests, with a base named as the engine accepted it, fails a check: the URL `http://registry.npmjs.org/codex.tgz` is not one a pin can hold: use `https://`, then ASCII letters and digits and `-._~:/@+`
            internal error: entered unreachable code: pin codex-0.167.0, written in code and its recipe made by the tests, with a base named as the engine accepted it, fails a check: the URL `https://registry.npmjs.org/$CODEX.tgz` is not one a pin can hold: use `https://`, then ASCII letters and digits and `-._~:/@+`
            internal error: entered unreachable code: pin codex-0.167.0, written in code and its recipe made by the tests, with a base named as the engine accepted it, fails a check: Docker may take `https://registry.npmjs.org/codex.git` for a Git repository's URL, which some of its versions clone without checking a checksum: name no `ADD` source that ends in `.git`, in any case
            internal error: entered unreachable code: pin codex-0.167.0, written in code and its recipe made by the tests, with a base named as the engine accepted it, fails a check: `2dc9aa871abb513c3539d20d025f701faf1974c46ee906bda3a1e1f5026d4434` is not a digest: use `sha256:` and 64 lowercase hexadecimal digits
            internal error: entered unreachable code: pin codex-0.167.0, written in code and its recipe made by the tests, with a base named as the engine accepted it, fails a check: `package/../vendor` is not a normalized relative path: use `/`-separated components, none of them empty, `.` or `..`, and no NUL byte
            internal error: entered unreachable code: pin codex-0.167.0, written in code and its recipe made by the tests, with a base named as the engine accepted it, fails a check: the path `bin/codex*` is not one a pin can hold: use ASCII letters and digits and `-._/`
            internal error: entered unreachable code: pin codex-0.167.0, written in code and its recipe made by the tests, with a base named as the engine accepted it, fails a check: the pin keeps no file: list the files it keeps
            internal error: entered unreachable code: pin codex-0.167.0, written in code and its recipe made by the tests, with a base named as the engine accepted it, fails a check: `bin/codex` comes after `codex-path/rg`: list the files once each, sorted by their paths' bytes
            internal error: entered unreachable code: pin codex-0.167.0, written in code and its recipe made by the tests, with a base named as the engine accepted it, fails a check: `bin/codex` comes after `bin/codex`: list the files once each, sorted by their paths' bytes
            internal error: entered unreachable code: pin codex-0.167.0, written in code and its recipe made by the tests, with a base named as the engine accepted it, fails a check: the base `` is not an image reference a recipe can hold: use ASCII letters and digits and `-._/:@`
            internal error: entered unreachable code: pin codex-0.167.0, written in code and its recipe made by the tests, with a base named as the engine accepted it, fails a check: the base `base\nRUN id` is not an image reference a recipe can hold: use ASCII letters and digits and `-._/:@`"#]],
    );
}

/// The file a pin keeps at `path` with `mode`.
const fn file(path: &'static str, mode: Mode) -> PinnedFile {
    PinnedFile { path, mode }
}

/// The message `pin.recipe(base)` panics with, observed by joining the thread that panicked, or
/// that it makes the recipe.
fn panic_message(pin: Pin, base: &'static str) -> String {
    let Err(payload) = thread::spawn(move || pin.recipe(base)).join() else {
        return format!("pin {}: recipe made", pin.name);
    };
    match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(_) => "a panic without a message".to_owned(),
    }
}
