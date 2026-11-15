//! Pumpkin's standard library: the files of the repository's `library/` directory, built into
//! the crate, which `pumpkin` serves as the files below `@pumpkin//`, so that the library a task
//! loads is the one its `pumpkin` was built and tested with.
//!
//! A harness is a directory of the library, `harness/<name>/`, such as `harness/codex/`: a
//! `.star` file of its name, which exports one function, and the `Dockerfile` that adds the
//! harness to an agent's image, which that function's `image()` builds with the agent's image as
//! the named context `base`. The function takes that image by position and, by keyword, at least
//! `model`, `effort`, `workspace`, `prompt`, `deadline_seconds` and `finalize_grace_seconds`, and
//! gives a dict of an agent role's `image`, `runtime` and `egress`.

// The files are a table, as Buck2 bundles a test cell
// (https://github.com/facebook/buck2/blob/e3dc98f84acf5f70c40b857819556baa7ec9c929/app/buck2_external_cells_bundled/src/lib.rs#L42-L77),
// rather than the list a build script writes, as it writes Buck2's prelude: a handful of files,
// each named once, and the tests fail if the table and the directory differ.
#![no_std]

/// Each file's path below `library/` and the bytes it holds, sorted by path.
macro_rules! files {
    ($($path:literal),* $(,)?) => {
        [$(($path, include_bytes!(concat!("../../../library/", $path)).as_slice())),*]
    };
}

/// Each file of the library, by its path below the library's root, such as
/// `harness/codex/codex.star`, sorted by the paths' bytes.
pub const FILES: [(&str, &[u8]); 6] = files![
    "harness/claude_code/claude_code.star",
    "harness/claude_code/layer/Dockerfile",
    "harness/codex/LICENSE",
    "harness/codex/catalog.star",
    "harness/codex/codex.star",
    "harness/codex/layer/Dockerfile",
];
