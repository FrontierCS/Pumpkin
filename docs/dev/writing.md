# Writing

The rules for Pumpkin's rustdoc, comments, commit messages and pull request texts.

- WRITE a doc only where the name, the type and the signature leave a caller room to go wrong:
  a unit, a precondition, who owns what, an invariant, a case left unspecified. A private item
  has none by default, and a test has none: a `//` of a line or two, only for a why
- WRITE a doc as the contract, in plain sentences: what the caller gets and must give, never how
  it works. Write `# Errors` and `# Panics` only for a condition the caller can act on
- KEEP a comment to the code it sits by: say the condition, never which callers, roles or
  features reach it, and never what another module does
- WRITE each rule once, at its home. Elsewhere, name it only where a reader would otherwise
  break it
- WRITE a why as a `//` at the line it explains: the failure it prevents, where a number comes
  from, the alternative turned down
- CITE what shaped a design: name the project, paper or document, and link it (code by a
  permalink at a full commit SHA)
- WRITE a module doc for the module's model and what it leaves to others, never a tour of its
  items
- TREAT help text (clap, xflags) and `schema::ir` docs as interface
- PREFER a type, a check or a test to prose
- NEVER write history or hedges
- CHANGE, in a commit about something else, only the docs and comments of the code it changes
- KEEP `ARCHITECTURE.md` to what changes slowly, as
  [rust-analyzer's](https://github.com/rust-lang/rust-analyzer/blob/55effabb762bbd0a21c5af530ea447bd6772881d/docs/book/src/contributing/architecture.md)
  does: a bird's-eye view, the entry points, a code map giving each crate's purpose, key types
  and boundary invariants, and the cross-cutting concerns. Name code there (a crate, a module,
  a type, a source file), never link it, as a link to code goes stale; link another document
- `cargo xtask ci` checks the rest: doc lengths, no doc on a test, doc form (first paragraph,
  punctuation, backticks, links), `# Panics` on a public function that can panic, no `TODO`,
  `FIXME`, `XXX` or code commented out
- WRITE commit subjects in lowercase imperative, with bodies that explain what and why to a
  reader without other context
- LIST schema changes in the pull request description
- SKETCH, in the pull request of a design whose cost grows with its input, what it costs in
  network, disk, memory and CPU, each by bandwidth and latency, before building it; then
  measure the claims the choice rests on
