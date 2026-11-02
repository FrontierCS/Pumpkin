- ALWAYS pass the checks before finishing: `cargo xtask ci`, and `cargo xtask ci --docker`,
  which also runs the tests that need a Docker engine, when a change touches code only those
  tests reach
- NEVER assume a clippy warning or a test failure is pre-existing
- PREFER running the narrowest relevant test while iterating
- ALWAYS find the nearest existing code and its tests before adding new code, and extend
  existing helpers instead of creating parallel ones
- PREFER the smallest change that fits the existing design
- TREAT a new dependency, crate or `pub` item as an architectural change, and say why in the
  pull request
- PREFER types that only hold valid values: parse input once at the boundary, and take the
  parsed types everywhere past it
- PREFER interfaces (flags, configuration, protocol messages, `pub` APIs) that express exactly
  what real runs need: a value fixed in every real run is a constant, not a parameter, and each
  setting has one way to set it
- PREFER command-line flags for configuration, and the environment only for secrets that must
  stay out of `argv`
- NEVER add a concurrency primitive (`Mutex`, `Arc`, an atomic, a channel, a thread) without
  the sharing that needs it; express single ownership in the types (`&mut`, an owned handle)
- PREFER moving branches up to the caller and loops down into functions that take a batch
- PREFER exhaustive `match` without wildcard arms, and destructuring structs in full when
  converting them
- PREFER `#[expect(lint, reason = "...")]` over `#[allow(lint)]`
- WRITE `unreachable!` for a path only a bug can reach, under
  `#[expect(clippy::unreachable, reason = "…")]` saying why it can't happen; never handle such a
  path. Handle, or return, only errors the world can cause
- WRITE error messages as what is wrong, then what is expected (`the program is empty: name one`)
- WRITE few tests, at the public boundary, through one `check` helper per test file that
  compares against an `expect![[...]]` snapshot; do not test what the type system or serde
  already guarantees
- START a snapshot as `expect![[""]]`, fill it with `UPDATE_EXPECT=1`, and read the diff
  before keeping it
- PREFER one integration test binary per crate; add modules, not files under `tests/`
- SET `test = false` on each library and binary target: tests live in the integration test
  binary, and `cargo test` would build each target again for an empty unit-test harness
- WRITE rustdoc beside the code, and each rule or invariant once, at its home (the type, the
  constructor, or the protocol's module doc); every other doc links to it
- WRITE rustdoc for a public item as its contract (what it does, its errors and panics), not
  how it works
- WRITE a module doc that says what the module is for and links to where its rules live; it
  never walks through an algorithm
- WRITE a module doc of at most 40 lines and an item's doc of at most 15, as `cargo xtask ci`
  checks
- WRITE the why of a state in a comment at that code, and no design rationale in rustdoc; a
  doc or a comment never narrates history or the conversation that produced the code
- NEVER restate what a name, a type or a signature says
- NEVER name other projects as precedent in docs, comments or commit messages; state the
  reason itself
- PREFER a type, a constructor check, an assertion, or a test that pins a format, to prose
- WRITE commit subjects in lowercase imperative, with bodies that explain what and why to a
  reader without other context
- LIST schema changes in the pull request description
