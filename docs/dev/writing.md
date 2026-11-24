# Writing

The rules for Pumpkin's rustdoc, comments, commit messages and pull request texts.

- WRITE a doc only where the name, the type and the signature leave a caller room to go wrong:
  a unit, a precondition, who owns what, an invariant, a case left unspecified. A private or
  `pub(crate)` item, field or variant, and a test helper, have no doc and no `//` above them, but
  for a unit, a precondition, an invariant or an owner that their type can't say, or a why. A doc
  that clap or xflags shows as help stays. A test has no doc: a `//` of a line or two, only for a
  why
- WRITE a doc as the contract, in plain sentences: what the caller gets and must give, never how
  it works. Write `# Errors` and `# Panics` only for a condition the caller can act on
- WRITE one idea per sentence, subject and verb first: split a sentence that chains clauses
  with commas or semicolons. Say an action with its verb, not a noun made from it, and drop a
  qualifier that changes nothing the reader does
- KEEP a comment to the code it sits by: say the condition, never which callers, roles or
  features reach it, and never what another module does
- USE the words the code uses: a term for a part of the code that no type, field or function
  names is stale, so rename it in the change that renames the code
- WRITE each rule once, at its home. Elsewhere, name it only where a reader would otherwise
  break it
- WRITE a why as a `//` at the line it explains: the failure it prevents, where a number comes
  from, the alternative turned down
- KEEP a measurement's result out of a comment and a commit message: where a why rests on a
  quantity, name it at the precision the reasoning needs; what was measured, with its result,
  goes in the pull request's description or an issue
- CITE what shaped a design: name the project, paper or document, and link it (code by a
  permalink at a full commit SHA)
- WRITE a module doc for the module's model and what it leaves to others, never a tour of its
  items
- TREAT help text (clap, xflags) and `schema::ir` docs as interface, and as the home of what a
  command, a flag or a key does: a guide names it, links its help or its `schema::ir` doc, and
  writes only how the parts fit
- PREFER a type, a check or a test to prose
- NEVER write history or hedges
- CHANGE, in a commit about something else, only the docs and comments of the code it changes
- KEEP `ARCHITECTURE.md` to what changes slowly, as
  [rust-analyzer's](https://github.com/rust-lang/rust-analyzer/blob/55effabb762bbd0a21c5af530ea447bd6772881d/docs/book/src/contributing/architecture.md)
  does: a bird's-eye view, the entry points, a code map giving each crate's purpose, key types
  and boundary invariants, and the cross-cutting concerns. Name code there (a crate, a module,
  a type, a source file), never link it, as a link to code goes stale; link another document,
  and embed a figure. An invariant there names a boundary between crates and why it matters,
  never how the code holds it, since the document isn't kept in step with the code, as
  [matklad's post](https://matklad.github.io/2021/02/06/ARCHITECTURE.md.html) advises
- DRAW a figure only of what `ARCHITECTURE.md` states in prose and changes slowly, such as the
  machines of a run or how the crates depend on one another, and never more than the prose says:
  an SVG in `docs/figures/`, its text kept as text in the system's fonts, in the style the others
  share, embedded beside that prose with alt text that says what it shows, and changed in the
  change that changes what it shows. `cargo xtask figures` writes the one drawn from the workspace
- `cargo xtask ci` checks the rest: doc lengths, no doc on a test and no `//` of over 2 lines on
  one, no doc whose first sentence only restates its item's name, doc form (first paragraph,
  punctuation, backticks, links), `# Panics` on a public function that can panic, no `TODO`,
  `FIXME`, `XXX` or code commented out, no line of a doc or comment past 100 columns but a table row
  or a single word, each Markdown document's words at its ceiling in `.config/tidy.toml`, no link in
  `ARCHITECTURE.md` to code or to a document the repository lacks, no image in a document, a
  Markdown image or an HTML `<img>`, but a figure in `docs/figures/` with alt text, no figure that
  no document embeds, a `<title>` and a `<desc>` in each figure and no `<image>`, `<script>` or
  `@font-face`, no name set as code in a figure that the workspace lacks, and the figure of the
  crates as `cargo xtask figures` draws it
- WRITE commit subjects in lowercase imperative, with bodies that explain what and why to a
  reader without other context
- LIST schema changes in the pull request description
- SKETCH, in the pull request of a design whose cost grows with its input, what it costs in
  network, disk, memory and CPU, each by bandwidth and latency, before building it; then
  measure the claims the choice rests on
