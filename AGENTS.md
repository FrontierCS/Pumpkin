- ALWAYS pass the checks before finishing: `cargo xtask ci`, and `cargo xtask ci --docker`,
  which also runs the tests that need a Docker engine, when a change touches code only those
  tests reach
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
- PREFER types that only hold valid values: parse input once at the boundary, and take the
  parsed types everywhere past it
- PREFER interfaces (flags, configuration, protocol messages, `pub` APIs) that express exactly
  what real runs need: a value fixed in every real run is a constant, not a parameter, and each
  setting has one way to set it
- PREFER flags for per-run choices, `pumpkin.toml` for per-machine facts, and the environment
  only for secrets that must stay out of `argv`
- NEVER add a concurrency primitive (`Mutex`, `Arc`, an atomic, a channel, a thread) without
  the sharing that needs it; express single ownership in the types (`&mut`, an owned handle)
- PREFER moving branches up to the caller and loops down into functions that take a batch
- PREFER exhaustive `match` without wildcard arms, and destructuring structs in full when
  converting them
- PREFER `#[expect(lint, reason = "...")]` over `#[allow(lint)]`
- PREFER a type that makes a bug-only path unrepresentable; where no type can express it, or one
  would cost more than it saves (such as a reshaped `pub` API or a dependency), WRITE
  `unreachable!` under `#[expect(clippy::unreachable, reason = "…")]` saying why only a bug can
  reach it; never handle such a path. Handle, or return, every error the world can cause, the
  kernel's and its security policy's included
- NEVER catch a panic outside tests: a panic is a bug, and it ends the whole process, since what
  it leaves behind can't be trusted; a binary that runs threads installs `stdx::abort_on_panic`
- WRITE `unsafe` only to make a system call `std` lacks, each use acknowledging the
  `unsafe_code` lint with a reason
- WRITE an error type of its own only for the cases a caller branches on, in this process,
  across a protocol or in a record (an enum, a nested `Result`, a reply's code), or for a check
  that reports every problem it finds in an input, each with where it is (`ParseErrors`); any
  other error is a message, in whichever of `String`, `io::Error` or `Box<dyn Error>` needs the
  fewest conversions, and code never branches on the text of a message Pumpkin wrote
- WRITE error messages as what is wrong, then what is expected
  (`the program is empty: name one`); passing an error up, add what was being done where the
  error doesn't say it, as `stdx::Context` does
- WRITE few tests, at the public boundary, through one `check` helper per test file that
  compares against an `expect![[...]]` snapshot; do not test what the type system or serde
  already guarantees
- START a snapshot as `expect![[""]]`, fill it with `UPDATE_EXPECT=1`, and read the diff
  before keeping it
- PREFER one integration test binary per crate; add modules, not files under `tests/`
- SET `test = false` on each library and binary target: tests live in the integration test
  binary, and `cargo test` would build each target again for an empty unit-test harness
- WRITE rustdoc beside the code, and each rule or invariant once, at its home (the type, the
  constructor, or the protocol's module doc); every other doc links to it, or names it for
  symbol search, as `ARCHITECTURE.md` does
- WRITE rustdoc for a public item as its contract (what it does, its errors and panics), not
  how it works
- WRITE a module doc that says what the module is for and links to where its rules live; it
  never walks through an algorithm
- WRITE a module doc of at most 40 lines and an item's doc of at most 15, as `cargo xtask ci`
  checks
- WRITE the why of a state in a comment at that code, and no design rationale in rustdoc; a
  doc or a comment never narrates history or the conversation that produced the code
- NEVER restate what a name, a type or a signature says
- PREFER a type, a constructor check, an assertion, or a test that pins a format, to prose
- WRITE commit subjects in lowercase imperative, with bodies that explain what and why to a
  reader without other context
- LIST schema changes in the pull request description
