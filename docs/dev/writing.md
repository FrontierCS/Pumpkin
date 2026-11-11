# Writing

The rules for Pumpkin's rustdoc, comments, commit messages and pull request texts.

- WRITE rustdoc beside the code, and each rule or invariant once, at its home (the type, the
  constructor, or the protocol's module doc); every other doc links to it, or names it for
  symbol search, as `ARCHITECTURE.md` does, and never restates it
- WRITE rustdoc as the item's contract, in the fewest sentences: what it does, its errors and
  its panics; never how it works
- WRITE a module doc that says what the module is for and links to where its rules live
- WRITE no doc on a test: its name and its snapshot say what it pins; a why that a reader can't
  see, such as a timing margin or a setup someone would delete, is a `//` comment of a line or
  two at the line it explains
- WRITE a doc on a test helper only for a contract its callers need, in a sentence or two
- WRITE the why of a state in a comment at that code, never in rustdoc, and nothing else in a
  comment: no restated behaviour of a tool or a library, no history, no precedent or allusion to
  another project, no claim that nothing checks a thing, no hedges or filler; say each idea once
- NEVER restate what a name, a type or a signature says
- KEEP a module doc within 25 lines and an item's within 9, and a test without one, as
  `cargo xtask ci` checks
- CUT a doc or a comment that breaks these rules, or shorten it to its one load-bearing
  sentence; don't reword it at length
- PREFER a type, a constructor check, an assertion, or a test that pins a format, to prose
- WRITE commit subjects in lowercase imperative, with bodies that explain what and why to a
  reader without other context
- LIST schema changes in the pull request description
- SKETCH, in the pull request of a design whose cost grows with its input, what it costs in
  network, disk, memory and CPU, each by bandwidth and latency, before building it; then
  measure the claims the choice rests on
