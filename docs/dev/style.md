# Style

The rules for writing Rust in Pumpkin: types, naming, interfaces, bounds, control flow,
assertions and errors.

- PREFER types that only hold valid values: parse input once at the boundary, and take the
  parsed types everywhere past it
- NAME a number by what it measures, its unit last (`payload_bytes`, `calls_count`): an `index`
  counts from 0, a `count` from 1 and a `size` in bytes, and a time past the boundary is a
  `Duration`. A mixed-up unit, or an index taken for a count, then looks wrong where it is written
- PREFER interfaces (flags, configuration, protocol messages, `pub` APIs) that express exactly
  what real runs need, as each parameter is a case for every caller and test to get right: a value
  fixed in every real run is a constant. Each setting has one way to set it: a flag for a per-run
  choice, `pumpkin.toml` for a per-machine fact, and the environment only for where the system
  keeps things, such as `HOME`, and for secrets that must stay out of `argv`
- PREFER moving branches up to the caller and loops down into functions that take a batch
- BOUND whatever input or time can grow, each queue, buffer, read, retry and recursion, by a
  limit from precedent or the protocol, named as a constant at its home; a loop meant never to
  end returns `!`. Otherwise the agent's code, or a stalled peer, grows it until the host runs out
- PREFER exhaustive `match` without wildcard arms, and destructuring structs in full when
  converting them, so that a new variant or field fails the build where code must decide on it
- PREFER a type that makes a bug-only path unrepresentable; where none can, or one would cost
  more than it saves (such as a reshaped `pub` API or a dependency), WRITE `unreachable!` rather
  than handle the path, as a handler that no run reaches hides the bug that does. Handle, or
  return, every error the world can cause, the kernel's and its security policy's included
- WRITE `assert!` for an invariant that no type can hold, such as a count within its limit or
  one event after another, most of all in code a simulation test drives, and
  `const _: () = assert!(…)` for a relation between constants that code relies on
- WRITE an error type of its own only for the cases a caller branches on, in this process,
  across a protocol or in a record (an enum, a nested `Result`, a reply's code), or for a check
  that reports every problem in an input with where it is (`ParseErrors`): each caller pays for
  one in conversions. Any other error is a message, in whichever of `String`, `io::Error` or
  `Box<dyn Error>` needs the fewest, and code never branches on the text of a message Pumpkin wrote
- WRITE error messages as what is wrong, then what is expected
  (`the program is empty: name one`); passing an error up, add what was being done where the
  error doesn't say it, as `stdx::Context` does
