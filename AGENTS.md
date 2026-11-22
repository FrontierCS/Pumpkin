- ALWAYS pass the checks before finishing: `cargo xtask ci`, and `cargo xtask live docker`,
  the tests that need a Docker engine, when a change touches code only those tests reach
- RUN `cargo xtask live ec2` and `cargo xtask live modal`, which cost money, at review of a
  change that only a real provider reaches, as `docs/dev/testing.md` says
- RUN `cargo xtask install-tools` only before working offline, or before a plain `cargo test` in
  a new checkout: each xtask command fetches the pinned tools it runs on first use
- RUN `cargo xtask mutants` on demand, as `docs/dev/testing.md` says
- NEVER assume a clippy warning or a test failure is pre-existing
- PREFER running the narrowest relevant test while iterating
- ALWAYS find the nearest existing code and its tests before adding new code, and extend
  existing helpers instead of creating parallel ones
- ALWAYS put new code where `ARCHITECTURE.md` places it, and update `ARCHITECTURE.md` in the
  same pull request only when a crate or module boundary, or an invariant it states, changes
- PREFER small, focused changes: one concern per pull request, nothing speculative
- PREFER fixing a misplaced boundary over working around it: when keeping a rule needs a
  patch on top (duplicated configuration, a check that keeps copies in step, a ban on what a
  crate can merely reach), move the code so the structure itself enforces the rule
- TREAT a new dependency, crate or `pub` item as an architectural change, and say why in the
  pull request
- NEVER add a concurrency primitive (`Mutex`, `Arc`, an atomic, a channel, a thread) without
  the sharing that needs it; express single ownership in the types (`&mut`, an owned handle)
- NEVER catch a panic outside tests: a panic is a bug, and it ends the whole process, since what
  it leaves behind can't be trusted; a binary that runs threads installs `stdx::abort_on_panic`
- NEVER write `unsafe`, which the workspace denies, with `stdx::signal::catch` its one expected
  use: make a system call `std` lacks through `rustix`

Before writing Rust, read `docs/dev/style.md`; before writing tests, `docs/dev/testing.md`;
before writing docs, comments, commit messages or PR texts, `docs/dev/writing.md`.
`ARCHITECTURE.md` says where code goes.
