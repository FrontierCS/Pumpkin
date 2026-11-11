# Testing

The rules for writing Pumpkin's tests and their snapshots.

- WRITE few tests, at the public boundary, through one `check` helper per test file that
  compares against an `expect![[...]]` snapshot; do not test what the type system or serde
  already guarantees
- START a snapshot as `expect![[""]]`, fill it with `UPDATE_EXPECT=1`, and read the diff
  before keeping it
- PREFER one integration test binary per crate; add modules, not files under `tests/`
- ASSUME that each test runs alone, in a process of its own, as cargo-nextest runs it in
  `cargo xtask ci`: tests share no per-process state
- WRITE a test's scratch into a `tempfile::TempDir` in the system's temporary directory: no other
  test shares it, and its paths fit a socket's, which holds 107 bytes at most
- RUN `cargo xtask mutants` at review of a change to the decision core or to the schema's
  refusals, and read each mutant it reports missed, a change that no test catches: ci doesn't
  run it
- SET `test = false` on each library and binary target: tests live in the integration test
  binary, and `cargo test` would build each target again for an empty unit-test harness; SET
  `doctest = false` too on each library whose docs hold no Rust example, which rustdoc would build
  an empty doctest run for
