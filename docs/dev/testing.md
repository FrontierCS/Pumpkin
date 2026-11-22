# Testing

The rules for writing Pumpkin's tests and their snapshots.

- WRITE few tests, at the public boundary, through one `check` helper per test file that
  compares against an `expect![[...]]` snapshot; do not test what the type system or serde
  already guarantees
- EARN each test: it pins what a user, a task or a later change relies on, and no other test pins
- PREFER testing a decision as a function of values to running it through a stand-in
- START a snapshot as `expect![[""]]`, fill it with
  `cargo xtask refill --package <crate> [<test>]`, and read the diff before keeping it
- SNAPSHOT each command's help, and each help topic's, as an SVG of its own in
  `crates/pumpkin/tests/it/help/`. Fill it with `cargo xtask refill`, and read the diff before
  keeping it
- ASSERT in a file's `check` helper what holds of every case, such as files left or modes; show
  in the snapshot what the test is about, and what departs from that
- MAKE the state that a reader of Pumpkin's own format reads with Pumpkin's writer, or build a
  small one in the test; CHECK IN only data a third party wrote, such as a vendored suite
- PREFER one integration test binary per crate; add modules, not files under `tests/`
- RUN `cargo xtask install-tools`, or an xtask command that runs tests such as `cargo xtask ci`,
  before a plain `cargo test` or `cargo nextest run` in a new checkout. `pumpkin`'s tests run the
  pinned formatter from `bin/buildifier`, which `.cargo/config.toml` names. Each of those
  commands links it there from the cache that checkouts share, and fetches it on first use
- ASSUME that each test runs alone, in a process of its own, as cargo-nextest runs it in
  `cargo xtask ci`: tests share no per-process state
- SPLIT cases that each start a process into tests of their own, which nextest runs at once
- WRITE a test's scratch into its `test_support::TestDir`: no other test shares it, its paths fit
  a socket's, which holds 107 bytes at most, and a test keeps only its last failure's; `stdx`'s
  tests, whose manifest names no Pumpkin crate, write theirs into a `tempfile::TempDir`
- RUN `cargo xtask mutants crates/decision crates/schema crates/route crates/retention
  crates/members` on demand, when tests of the decision core, the schema's refusals, the route's
  request rules, the controller's prune or the members' list's refusals are deleted or rewritten.
  The first three crates guard the operator's key. `retention` decides what the prune removes.
  `members` guards the controller's forced command. Run it with `CARGO_MUTANTS_JOBS=2` at most.
  Read each mutant it reports missed, as ci doesn't run it. Where the change leaves the source
  below those paths as it is, mutate that source whole, and name each mutant that `origin/main`'s
  tests catch and the change's tests miss
- KEEP `cargo xtask ci` pure: its tests reach no Docker engine, network or cloud; temporary
  files, Pumpkin's own processes and loopback sockets are fine. IGNORE each test that needs a Docker engine, the static `sandboxd` or
  a server elsewhere (`#[ignore = "…: run cargo xtask live docker"]`)
- REFRESH the EC2 provider's table of AWS's identity certificates, when AWS rotates one or adds
  a region, with `cargo xtask aws-certificates`, which fetches AWS's page and rewrites the table
  in place: it is the one command that reaches the network for it, and `cargo xtask ci` only
  tests the table committed. Read the diff, and refill the `identity_certificates` snapshot of
  `providers/ec2` with `cargo xtask refill --package ec2 identity_certificates`
- SET each bound that a test chooses at its floor, and script the clock (`SimHost`) for a case
  past a bound; wait on the real clock only for a bound that production states in whole seconds
- NEVER test against an invented model of a cloud or a service, which pins what the model answers,
  and NEVER add a stand-in that answers for another program either, though one may record that it
  ran: what a program really answers is tested by `cargo xtask live`, or not at all; test a provider
  by `cargo xtask live ec2` and `cargo xtask live modal` instead; RUN them at review of a change to
  a provider, `crates/aws`, `crates/tls`, the gateway's meeting, `sandboxd`'s listen, dial or user
  drop, `ec2-boot`, the EC2 user data or the Modal helper, and give the run's id in the pull request
- RUN `cargo xtask live examples --status`, with its `--model` and `--binding`, which costs money,
  on the commit of a release before its tag is pushed, as `cargo xtask release <tag> --publish`
  refuses the tag without the success that it posts
- SET `test = false` on each library and binary target: tests live in the integration test
  binary, and `cargo test` would build each target again for an empty unit-test harness; SET
  `doctest = false` too on each library whose docs hold no Rust example, which rustdoc would build
  an empty doctest run for
- `cargo xtask ci` checks the rest: each test ends within 5 s, but those that
  `.config/nextest.toml` names with why, and no file but Rust sits under a `tests/` directory
  outside `crates/lang`'s vendored suites and `.star` fixtures, and `crates/pumpkin`'s help
  snapshots in `tests/it/help/`
