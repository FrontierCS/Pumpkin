//! Codex as a harness: [`CODEX_0_167_0`] pins the Codex that agents run.

use crate::{Mode, Pin, PinnedFile};

/// Codex 0.167.0 for x86-64 Linux, from npm's platform tarball, `@openai/codex@0.167.0-linux-x64`.
///
/// It keeps `bin/codex`, Codex itself, which finds its other files from its own executable's
/// path: `codex-package.json`, beside its `bin` directory, marks the package's layout.
/// `bin/codex-code-mode-host` is the process Codex starts for code mode, and `codex-path/rg` is
/// ripgrep, which Codex puts on its tools' `PATH`. Every binary is static, so the harness needs
/// nothing from the image it is added to.
pub const CODEX_0_167_0: Pin = Pin {
    name: "codex-0.167.0",
    url: "https://registry.npmjs.org/@openai/codex/-/codex-0.167.0-linux-x64.tgz",
    sha256: "sha256:2dc9aa871abb513c3539d20d025f701faf1974c46ee906bda3a1e1f5026d4434",
    dir: "package/vendor/x86_64-unknown-linux-musl",
    // The tarball's `codex-resources` stays out: its `bwrap` is Codex's own sandbox, for which
    // the agent's sandbox stands in, and its voice host serves spoken input, which an unattended
    // agent has none of.
    files: &[
        PinnedFile {
            path: "bin/codex",
            mode: Mode::Executable,
        },
        PinnedFile {
            path: "bin/codex-code-mode-host",
            mode: Mode::Executable,
        },
        PinnedFile {
            path: "codex-package.json",
            mode: Mode::Readable,
        },
        PinnedFile {
            path: "codex-path/rg",
            mode: Mode::Executable,
        },
    ],
};
