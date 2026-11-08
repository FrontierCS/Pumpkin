# Writing

The rules for Pumpkin's rustdoc, comments, commit messages and pull request texts.

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
