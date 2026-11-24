- ALWAYS pass the checks before finishing: `cargo xtask ci`; `cargo xtask live docker`, the
  tests that need a Docker engine, when a change touches code only those tests reach; and
  `cargo xtask live controller` and `cargo xtask live controller --lost` when a change touches
  what `docs/dev/testing.md` names for them. NEVER assume that a clippy warning or a test failure
  is pre-existing
- PREFER running the narrowest relevant test while iterating
- ALWAYS find the nearest existing code and its tests before adding new code, and extend
  existing helpers instead of creating parallel ones. A helper that a second crate's tests need
  moves into `crates/test-support`
- ALWAYS put new code where `ARCHITECTURE.md` places it
- PREFER small, focused changes: one concern per pull request, nothing speculative, and no edit
  of the docs and comments of code the change leaves as it is, but for a term that it renames
- PREFER fixing a misplaced boundary over working around it: where a rule needs a patch on top,
  such as duplicated configuration or a check that keeps copies in step, move the code so that
  the structure itself enforces the rule, as a patch holds only while every later change keeps it
- TREAT a new dependency, crate or `pub` item as an architectural change, and say why in the
  pull request
- NEVER add a concurrency primitive (`Mutex`, `Arc`, an atomic, a channel, a thread) without
  the sharing that needs it; express single ownership in the types (`&mut`, an owned handle). A
  reader takes a primitive to mean that something else holds the value, and looks for it

Before writing Rust, read `docs/dev/style.md`; before writing or running tests,
`docs/dev/testing.md`; before writing docs, comments, commit messages or PR texts,
`docs/dev/writing.md`.
`ARCHITECTURE.md` says where code goes.
