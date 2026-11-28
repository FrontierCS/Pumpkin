# Writing

The rules for Pumpkin's rustdoc, comments, commit messages and pull request texts. A pull
request's text answers `.github/PULL_REQUEST_TEMPLATE.md`.

- PREFER code that explains itself to prose: a name, a type, a smaller function, a check or a
  test. Assert an invariant that the reader must know instead of commenting it
- ALWAYS say why: the reason for a decision, the failure it prevents, where a number comes from,
  the alternative turned down. Show your workings: the reasoning that makes the code right
- NEVER narrate the steps that the code takes
- DELETE a comment that would change if the code changed and its purpose didn't
- SAY how a test works: open it with a doc that states its goal and its method
- WRITE a comment in full sentences. A comment at the end of a line may be a phrase
- WRITE a why as a `//` at the line it explains. Say the condition, not which callers, roles or
  features reach it. Name a quantity at the precision that the reasoning needs
- PUT the explanation of a protocol, an algorithm or a design in the doc of the module or item
  that it shapes. Keep a `//` in a function body to a line or two
- CITE what shaped a design or a number: name the project, paper or document, and link it
- WRITE a doc only where the name, the type and the signature leave a caller room to go wrong.
  Examples are a unit, a precondition, who owns what, an invariant and a case left unspecified
- WRITE a doc as the contract: what the caller gets and must give, never how it works. Write
  `# Errors` only for a condition that the caller can act on
- WRITE a module doc for the module's model and what it leaves to others, never a tour of its
  items
- LIMIT the docs of a test helper, and of an item, field or variant private to its crate. Give
  one only what its type can't say, or a why. Help that clap or xflags shows is exempt
- TREAT help text (clap, xflags) and `schema::ir` docs as interface, and as the home of what a
  command, a flag or a key does
- WRITE in a guide only how the parts fit. A guide names a command, a flag or a key, and links
  its help or its `schema::ir` doc
- WRITE one idea per sentence, subject and verb first. Split a sentence that chains clauses with
  commas or semicolons. `cargo xtask ci` caps a sentence at 25 words and rejects a semicolon
  that chains clauses, in each tracked Markdown document and each Rust comment
- SAY an action with its verb, not a noun made from it
- DROP a qualifier that changes nothing the reader does
- NEVER write history or hedges
- WRITE each fact once, at its home. Name it elsewhere only where a reader would otherwise
  break it
- USE the code's words. Rename a term in the change that renames its code
- WRITE `ARCHITECTURE.md` as Pumpkin's design: the problem it solves, an overview, each design
  decision with its motivation, and the references that shaped them. End it with a short map of
  the crates
- KEEP `ARCHITECTURE.md` to what changes slowly. Change it when a change makes, reverses or
  reshapes a design decision, or adds, removes or renames a crate. Then raise its ceiling in
  `.config/tidy.toml` to its new word count. Put any other new fact in the code it describes
- STATE a boundary between crates and why it matters, never how the code holds it
- DRAW a figure only of something that `ARCHITECTURE.md` states in prose and that changes
  slowly. Never draw more than the prose says, and draw in the style that the other figures
  share. Change a figure in the change that changes what it shows
- WRITE commit subjects in lowercase imperative, with bodies that explain what and why to a
  reader without other context
- PUT what was measured, with its result, in the pull request or an issue, never in a comment
  or a commit message. Put the weighing of a design there too: the precedents and alternatives
- SKETCH what a design costs, in its pull request and before building it, when its cost grows
  with its input. Give the cost in network, disk, memory and CPU, each by bandwidth and latency.
  Then measure the claims the choice rests on
