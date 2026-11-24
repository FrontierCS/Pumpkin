# Testing

The rules for writing Pumpkin's tests and their snapshots.

- WRITE few tests, at the public boundary, through one `check` helper per test file that
  compares against an `expect![[...]]` snapshot
- PIN in each test a behaviour that a user, a task or a later change relies on, and that no
  other test pins. Never test what the type system or serde guarantees
- PREFER testing a decision as a function of values to running it through a stand-in
- START a snapshot as `expect![[""]]`, fill it with
  `cargo xtask refill --package <crate> [<test>]`, and read the diff before keeping it
- SNAPSHOT the help of each command that a person runs, and of each help topic, as an SVG in
  `crates/pumpkin/tests/it/help/`. A command that only a unit runs, such as a systemd service or
  sshd's forced command, has no snapshot
- ASSERT in a file's `check` helper what holds of every case, such as files left or modes. Show
  in the snapshot what the test is about, and what departs from that
- MASK in a file's `check` helper each value that the test isn't about. Examples are a fixture's
  length, a case's index, a protocol version and a clock reading. Mask a dependency's version or
  wording too, as no user reads it
- SET `test = false` on each library and binary, and `doctest = false` on each library whose docs
  hold no Rust example. Each extra target adds build time
- KEEP `cargo xtask ci` pure: its tests reach no Docker engine, network or cloud; temporary
  files, Pumpkin's own processes and loopback sockets are fine. IGNORE each test that needs a
  Docker engine, the static `sandboxd` or a server elsewhere
  (`#[ignore = "…: run cargo xtask live docker"]`)
- SET each bound that a test chooses at its floor, and script the clock (`SimHost`) for a case
  past a bound; wait on the real clock only for a bound that production states in whole seconds
- NEVER test against an invented model of a cloud or a service, which pins what the model answers,
  and NEVER add a stand-in that answers for another program either, though one may record that it
  ran: what a program really answers is tested by `cargo xtask live`, or not at all. The one
  exception is a documented network API that Pumpkin speaks as a client, such as a model provider's
  wire format. A stand-in may serve such an API
- COUNT a live suite's run as a behaviour's test: a behaviour that it verifies needs no test
  through a stand-in
- RUN `cargo xtask live ec2` and `cargo xtask live modal` at review of a change to a provider,
  `crates/aws`, `crates/tls` or `ec2-boot`. Run them too for a change to the gateway's meeting,
  `sandboxd`'s listen, dial or user drop, the EC2 user data or the Modal helper. Both suites cost
  money
- RUN `cargo xtask live controller` and `cargo xtask live controller --lost`, which need a Docker
  engine alone, at review of a change to `pumpkin`'s `controller`, `host`, `job`, `serve` or `link`,
  or to `deploy/docker` or `deploy/controller`
- RUN `cargo xtask mutants` on demand, when the tests of a crate that it mutates by default are
  deleted or rewritten. Run it with `CARGO_MUTANTS_JOBS=2` at most. Read each mutant that it
  reports missed, as ci doesn't run it
