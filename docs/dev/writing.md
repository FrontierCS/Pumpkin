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
- `cargo xtask ci` checks the rest: doc lengths, no doc on a test and no `//` of over 2 lines
  on one, no doc whose first sentence only restates its item's name, doc form (first paragraph,
  punctuation, backticks, links), `# Panics` on a public function that can panic, no `TODO`,
  `FIXME`, `XXX` or code commented out, no line of a doc or comment past 100 columns but a table
  row or a single word, and no link in `ARCHITECTURE.md` to code or to a document the
  repository lacks
- WRITE commit subjects in lowercase imperative, with bodies that explain what and why to a
  reader without other context
- LIST schema changes in the pull request description
- SKETCH, in the pull request of a design whose cost grows with its input, what it costs in
  network, disk, memory and CPU, each by bandwidth and latency, before building it; then
  measure the claims the choice rests on

## Examples

Some of the rules above, each with a case from this repository: what was written, then what
replaced it, in the form of
[rust-analyzer's](https://github.com/rust-lang/rust-analyzer/blob/55effabb762bbd0a21c5af530ea447bd6772881d/docs/book/src/contributing/style.md)
style guide.

### WRITE a doc only where the name, the type and the signature leave a caller room …

```rust
// BAD
/// Whether `holder` holds a lease that hasn't ended.
pub(super) fn holds(&self, holder: Caller) -> bool {

// GOOD
pub(super) fn holds(&self, holder: Caller) -> bool {
```

**Rationale:** a private item's reader has its body below; a doc there is one more text that a
change to the body must keep true.

### WRITE a doc as the contract …

```rust
// BAD
/// The most the container's log may hold, in bytes, in one file. The log is kept on the
/// host's disk, outside the container's other limits, so it is capped whatever writes there, …
const LOG_BYTES: u64 = 1 << 20;

// GOOD
/// The most the container's log may hold, in bytes, in one file.
// The log is kept on the host's disk, outside the container's other limits, so it is capped …
const LOG_BYTES: u64 = 1 << 20;
```

**Rationale:** an editor shows the doc wherever the item is used, so the reasoning buries what a
user needs; at the line, it reaches the one who changes the number.

### KEEP a comment to the code it sits by …

```rust
// BAD
// A run that has ended has no lease ready: the work phase's end closes the agent's and
// its calls', and the run ends only once the score's call's have ended.

// GOOD
// A run that has ended has no lease ready: the work phase's end closes every lease, and
// the run ends only once the final job's have ended.
```

**Rationale:** a list of who reaches a condition is true until another caller reaches it, and
nothing fails when it stops being true; the condition holds as long as the code does.

### WRITE each rule once, at its home …

BAD, in `pumpkin help config`, then in `deploy/aws/README.md`:

> They run `aws`, AWS CLI v2, with the Session Manager plugin, and `ssh`, `rsync`, `tar`,
> `ssh-keygen` and `git`, from PATH; …

> The person's machine, Linux on x86-64, needs AWS CLI v2, the Session Manager plugin, `ssh`,
> `rsync`, `tar` and `git`.

GOOD, in the README:

> … `pumpkin help config` says what each does, and which programs the person's machine, Linux on
> x86-64, needs.

**Rationale:** the two lists disagreed, `ssh-keygen` in the help alone, and a reader of either
can't tell which is right; a change to the programs must find every copy.

### WRITE a why as a `//` at the line it explains …

```rust
// BAD
// An SSM session carries about 0.73 MB of what `ssh` writes a second, so zlib's
// ratio is a gain in speed: measured through a controller, a task's datasets went
// 2.1x as fast, and a tree of 7,267 source files 1.9x. …
"Compression=yes",

// GOOD
// An SSM session carries under 1 MB a second, far less than zlib compresses in one,
// so what compression saves in bytes it saves in time. …
"Compression=yes",
```

**Rationale:** a measurement holds for one run, and a reader can't check it or tell when it
stopped holding; the bound the reasoning needs stays true, and the measurement goes in the pull
request, with what was measured.

### WRITE a module doc for the module's model …

```rust
// BAD
//! [`Engine`] runs `docker` as an ordinary user against the engine of the context that the
//! operator names ([`ContextName`]), which nothing in the environment overrides, and reports it
//! ([`Report`]). It builds images ([`Engine::build`]), writes a recipe's fetch stage
//! ([`Engine::lock`]), says which of its images a prune removes ([`prune`]), loads the image of a
//! `sandboxd` ([`Engine::load_sandboxd`]) and pushes ([`Pusher::push`]); …

// GOOD
//! [`Engine`] runs `docker` as an ordinary user against the engine of the context that the
//! operator names ([`ContextName`]), which nothing in the environment overrides. Outside the
//! engine, it writes only in the directory it is given. …
```

**Rationale:** rustdoc lists a module's items below its doc; a tour says them again, goes stale
with each item added or renamed, and crowds out the model, which no list shows.

### NEVER write history or hedges

```rust
// BAD
// Each start is attempted once (a failed start isn't retried today), which is why the hard
// end counts one startup per start. …

// GOOD
// Each start is attempted once (a failed start isn't retried), which is why the hard
// end counts one startup per start. …
```

**Rationale:** "today" tells a reader nothing of the code, and makes them ask whether a change is
planned, and when.
