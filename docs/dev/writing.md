# Writing

The rules for Pumpkin's rustdoc, comments, commit messages and pull request texts. A pull
request's text answers `.github/PULL_REQUEST_TEMPLATE.md`.

- WRITE a doc only where the name, the type and the signature leave a caller room to go wrong:
  a unit, a precondition, who owns what, an invariant, a case left unspecified. Write it as the
  contract, what the caller gets and must give, never how it works, with `# Errors` only for a
  condition the caller can act on. A module doc gives the module's model and what it leaves to
  others, never a tour of its items. An item, field or variant private to its crate, and a test
  helper, get a doc or a `//` only for what their type can't say, or for a why, but for a doc
  that clap or xflags shows as help
- WRITE one idea per sentence, subject and verb first: split a sentence that chains clauses
  with commas or semicolons. Say an action with its verb, not a noun made from it, drop a
  qualifier that changes nothing the reader does, and write no history or hedges
- WRITE each fact once, at its home, and name it elsewhere only where a reader would otherwise
  break it. PREFER a type, a check or a test to prose. Keep a comment to the code it sits by:
  say the condition, never which callers, roles or features reach it, nor what another module
  does. Use the code's words, and rename a term in the change that renames its code
- WRITE a why as a `//` at the line it explains: the failure it prevents, where a number comes
  from, the alternative turned down. Where it rests on a quantity, name it at the precision the
  reasoning needs. What was measured, with its result, goes in the pull request or an issue,
  never in a comment or a commit message
- CITE what shaped a design: name the project, paper or document, and link it
- TREAT help text (clap, xflags) and `schema::ir` docs as interface, and as the home of what a
  command, a flag or a key does: a guide names it, links its help or its `schema::ir` doc, and
  writes only how the parts fit
- KEEP `ARCHITECTURE.md` to what changes slowly, as
  [rust-analyzer's](https://github.com/rust-lang/rust-analyzer/blob/55effabb762bbd0a21c5af530ea447bd6772881d/docs/book/src/contributing/architecture.md)
  does. CHANGE it only when a crate is added, removed or renamed, a name it uses is renamed, or a
  change breaks a boundary invariant it states; any other new fact goes in the code it describes.
  A change that adds a crate raises the document's exception in `.config/tidy.toml` to its count.
  An invariant there names a boundary between crates and why it matters, never how the code
  holds it, as [matklad's post](https://matklad.github.io/2021/02/06/ARCHITECTURE.md.html)
  advises: the document isn't kept in step with the code
- DRAW a figure only of what `ARCHITECTURE.md` states in prose and changes slowly, never more
  than the prose says and in the style the others share, and change it in the change that changes
  what it shows
- WRITE commit subjects in lowercase imperative, with bodies that explain what and why to a
  reader without other context
- SKETCH, in the pull request of a design whose cost grows with its input, what it costs in
  network, disk, memory and CPU, each by bandwidth and latency, before building it; then
  measure the claims the choice rests on
