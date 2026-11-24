# Writing

The rules for Pumpkin's rustdoc, comments, commit messages and pull request texts. A pull
request's text answers `.github/PULL_REQUEST_TEMPLATE.md`.

- WRITE a doc only where the name, the type and the signature leave a caller room to go wrong.
  Examples are a unit, a precondition, who owns what, an invariant and a case left unspecified
- WRITE a doc as the contract: what the caller gets and must give, never how it works. Write
  `# Errors` only for a condition that the caller can act on
- WRITE a module doc for the module's model and what it leaves to others, never a tour of its
  items
- LIMIT the docs of a test helper, and of an item, field or variant private to its crate. Give
  one of them a doc or a `//` only for what its type can't say, or for a why. The limit doesn't
  apply to a doc that clap or xflags shows as help
- WRITE each sentence subject and verb first. `cargo xtask ci` caps the words of each sentence in a
  tracked Markdown document, and rejects a semicolon that chains clauses there
- SAY an action with its verb, not a noun made from it
- DROP a qualifier that changes nothing the reader does
- NEVER write history or hedges
- WRITE each fact once, at its home. Name it elsewhere only where a reader would otherwise
  break it
- PREFER a type, a check or a test to prose
- KEEP a comment to the code it sits by. Say the condition, not which callers, roles or
  features reach it, and not what another module does
- USE the code's words. Rename a term in the change that renames its code
- WRITE a why as a `//` at the line it explains: the failure it prevents, where a number comes
  from, the alternative turned down. When a why rests on a quantity, name the quantity at the
  precision that the reasoning needs
- PUT what was measured, with its result, in the pull request or an issue, never in a comment
  or a commit message
- CITE what shaped a design: name the project, paper or document, and link it
- TREAT help text (clap, xflags) and `schema::ir` docs as interface, and as the home of what a
  command, a flag or a key does
- WRITE in a guide only how the parts fit. A guide names a command, a flag or a key, and links
  its help or its `schema::ir` doc
- KEEP `ARCHITECTURE.md` to what changes slowly, as
  [rust-analyzer's](https://github.com/rust-lang/rust-analyzer/blob/55effabb762bbd0a21c5af530ea447bd6772881d/docs/book/src/contributing/architecture.md)
  does
- CHANGE `ARCHITECTURE.md` only when a change adds, removes or renames a crate, renames a name it
  uses, or breaks a boundary invariant it states. Put any other new fact in the code it describes
- RAISE the ceiling of `ARCHITECTURE.md` in `.config/tidy.toml` to its new word count in a change
  that adds a crate
- STATE an invariant in `ARCHITECTURE.md` as a boundary between crates and why it matters, never
  how the code holds it, as
  [matklad's post](https://matklad.github.io/2021/02/06/ARCHITECTURE.md.html) advises. The
  document isn't kept in step with the code
- DRAW a figure only of something that `ARCHITECTURE.md` states in prose and that changes
  slowly. Never draw more than the prose says, and draw in the style that the other figures share
- CHANGE a figure in the change that changes what it shows
- WRITE commit subjects in lowercase imperative, with bodies that explain what and why to a
  reader without other context
- SKETCH what a design costs, in its pull request and before building it, when its cost grows
  with its input. Give the cost in network, disk, memory and CPU, each by bandwidth and latency.
  Then measure the claims the choice rests on
