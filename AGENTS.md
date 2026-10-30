- ALWAYS pass the checks before finishing: `cargo xtask ci`
- NEVER assume a clippy warning or a test failure is pre-existing
- PREFER running the narrowest relevant test while iterating
- ALWAYS find the nearest existing code and its tests before adding new code, and extend
  existing helpers instead of creating parallel ones
- PREFER the smallest change that fits the existing design
- TREAT a new dependency, crate or `pub` item as an architectural change, and say why in the
  pull request
- PREFER types that only hold valid values: parse input once at the boundary, and take the
  parsed types everywhere past it
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
- WRITE rustdoc beside the code, and link to a fact's home instead of repeating it
- WRITE comments that explain current behavior and its rationale; never narrate history or
  the conversation that produced the code
- WRITE commit subjects in lowercase imperative, with bodies that explain what and why to a
  reader without other context
- LIST schema changes in the pull request description
