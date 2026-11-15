# Testing

The rules for writing Pumpkin's tests and their snapshots.

- WRITE few tests, at the public boundary, through one `check` helper per test file that
  compares against an `expect![[...]]` snapshot; do not test what the type system or serde
  already guarantees
- START a snapshot as `expect![[""]]`, fill it with
  `cargo xtask refill --package <crate> [<test>]`, which rewrites the snapshots of this checkout
  alone and prints `git diff --stat`, and read the diff before keeping it
- PREFER one integration test binary per crate; add modules, not files under `tests/`
- ASSUME that each test runs alone, in a process of its own, as cargo-nextest runs it in
  `cargo xtask ci`: tests share no per-process state
- WRITE a test's scratch into a `tempfile::TempDir` in the system's temporary directory: no other
  test shares it, and its paths fit a socket's, which holds 107 bytes at most
- RUN `cargo xtask mutants crates/decision crates/schema` at review of a change to the decision
  core or to the schema's refusals, which mutates the change in those crates alone, and read each
  mutant it reports missed, a change that no test catches: ci doesn't run it
- KEEP `cargo xtask ci` pure: its tests reach no Docker engine, network or cloud, and wait on no
  real clock; temporary files, Pumpkin's own processes and loopback sockets are fine. IGNORE each
  test that needs a Docker engine, the static `sandboxd` or a server elsewhere (`#[ignore = "…:
  run cargo xtask live docker"]`): `cargo xtask live docker` builds that `sandboxd` and runs the
  ignored tests
- NEVER test against an invented model of a cloud or a service, which pins what the model answers:
  `cargo xtask live ec2` and `cargo xtask live modal` run `examples/smoke` through the controller,
  probe each sandbox's confinement, and check that nothing of the run is left; RUN them at review
  of a change to a provider, `crates/aws`, `crates/tls`, the gateway's meeting, `sandboxd`'s
  listen, dial or user drop, `ec2-boot`, the EC2 entry or the Modal helper, and give the run's id
  in the pull request
- SET `test = false` on each library and binary target: tests live in the integration test
  binary, and `cargo test` would build each target again for an empty unit-test harness; SET
  `doctest = false` too on each library whose docs hold no Rust example, which rustdoc would build
  an empty doctest run for
