//! Pumpkin's standard library: the files of the repository's `library/` directory, built into
//! the crate, which `pumpkin` serves as the files below `@pumpkin//`, so that the library a task
//! loads is the one its `pumpkin` was built and tested with.
//!
//! A harness is a directory of the library, `harness/<name>/`, such as `harness/codex/`: a
//! `.star` file of its name, which exports one function, and the `Dockerfile` that adds the
//! harness to an agent's image, which that function's `image()` builds with the agent's image as
//! the named context `base`. The function gives a dict of an agent role's `image` and `runtime`,
//! which holds the agent's `egress`. Every harness's function takes `base` by position and these
//! by keyword, each in the same sense, and its own keywords, which its file documents with what
//! it adds to these:
//!
//! - `model`: the model's name, which the route names, and the model the harness calls.
//! - `effort`: how hard the model reasons, one of the values the harness's file lists.
//! - `workspace`: the absolute path the harness works in, which holds its home.
//! - `prompt`: the task the harness works on, a string.
//! - `finalize_grace_seconds`: how long the harness may keep running after a call ends its work.
//! - `max_output_tokens`: how many tokens one of the model's answers may hold, a positive int.
//! - `context_tokens`: how many tokens the model's context window holds, a positive int.
//!
//! A harness that uses either requires it: its own value for a model it doesn't know is a guess.
//!
//! `harness/node/` holds no harness: its `node()` gives the image of the Node.js that harnesses
//! written in JavaScript run on, which their recipes take as a named context.

// The arguments every harness takes are stated once, so that a task picks a harness by naming its
// function and its own keywords, as Bazel states the attributes its rules share on one page
// (https://bazel.build/reference/be/common-definitions) and each rule's page only its own.

#![no_std]

macro_rules! files {
    ($($path:literal),* $(,)?) => {
        [$(($path, include_bytes!(concat!("../../../library/", $path)).as_slice())),*]
    };
}

// The files are a table, as Buck2 bundles a test cell
// (https://github.com/facebook/buck2/blob/e3dc98f84acf5f70c40b857819556baa7ec9c929/app/buck2_external_cells_bundled/src/lib.rs#L42-L77),
// rather than the list a build script writes, as it writes Buck2's prelude: a handful of files,
// each named once, and the tests fail if the table and the directory differ.
/// Each file of the library, by its path below the library's root, such as
/// `harness/codex/codex.star`, sorted by the paths' bytes.
pub const FILES: [(&str, &[u8]); 16] = files![
    "harness/claude_code/claude_code.star",
    "harness/claude_code/layer/Dockerfile",
    "harness/codex/LICENSE",
    "harness/codex/catalog.star",
    "harness/codex/codex.star",
    "harness/codex/layer/Dockerfile",
    "harness/node/layer/Dockerfile",
    "harness/node/node.star",
    "harness/pi/layer/Dockerfile",
    "harness/pi/layer/package-lock.json",
    "harness/pi/layer/package.json",
    "harness/pi/pi.star",
    "harness/qwen_code/layer/Dockerfile",
    "harness/qwen_code/qwen_code.star",
    "harness/zcode/layer/Dockerfile",
    "harness/zcode/zcode.star",
];
