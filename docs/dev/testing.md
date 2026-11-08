# Testing

The rules for writing Pumpkin's tests and their snapshots.

- WRITE few tests, at the public boundary, through one `check` helper per test file that
  compares against an `expect![[...]]` snapshot; do not test what the type system or serde
  already guarantees
- START a snapshot as `expect![[""]]`, fill it with `UPDATE_EXPECT=1`, and read the diff
  before keeping it
- PREFER one integration test binary per crate; add modules, not files under `tests/`
- SET `test = false` on each library and binary target: tests live in the integration test
  binary, and `cargo test` would build each target again for an empty unit-test harness
