# Style

The rules for writing Rust in Pumpkin: types, naming, interfaces, bounds, control flow,
assertions and errors.

- PREFER types that only hold valid values: parse input once at the boundary, and take the
  parsed types everywhere past it
- NAME a number by what it measures, its unit last (`payload_bytes`, `calls_count`): an `index`
  counts from 0, a `count` from 1 and a `size` in bytes, and a time past the boundary is a
  `Duration`. With these names, a mixed-up unit, or an index taken for a count, looks wrong where
  it is written
- PREFER interfaces (flags, configuration, protocol messages, `pub` APIs) that express exactly
  what real runs need. Each parameter is a case that every caller and test must get right. A value
  that stays the same in every real run is a constant
- GIVE each setting one way to set it. Use a flag for a per-run choice, and `pumpkin.toml` for a
  per-machine fact. Use the environment only for where the system keeps things, such as `HOME`,
  and for secrets that must stay out of `argv`
- PREFER moving branches up to the caller and loops down into functions that take a batch
- BOUND whatever input or time can grow: each queue, buffer, read, retry and recursion. Take each
  limit from precedent or the protocol, and name it as a constant at its home. Without a bound,
  the agent's code or a stalled peer grows it until the host runs out. A loop meant never to end
  returns `!`
- PREFER exhaustive `match` without wildcard arms, and destructuring structs in full when
  converting them. That way, a new variant or field fails the build where code must decide on it
- PREFER a type that makes a bug-only path unrepresentable
- WRITE `unreachable!` on a bug-only path that no type can make unrepresentable. Write it too
  where such a type would cost more than it saves, such as a reshaped `pub` API or a dependency.
  Don't handle the path. Only a bug reaches it, and a handler would hide that bug
- HANDLE or return every error that the world can cause, including the kernel's and its security
  policy's
- WRITE `assert!` for an invariant that no type can hold, such as a count within its limit or
  one event after another. Write it most of all in code that a simulation test drives. Write
  `const _: () = assert!(…)` for a relation between constants that code relies on
- WRITE an error type of its own only for the cases that a caller branches on. The branch can be
  in this process, across a protocol or in a record (an enum, a nested `Result`, a reply's code).
  A check that reports every problem in an input, each with where it is, gets one too
  (`ParseErrors`). Each caller pays for an error type in conversions. Make any other error a
  message, in whichever of `String`, `io::Error` or `Box<dyn Error>` needs the fewest conversions
- NEVER branch on the text of a message that Pumpkin wrote
- WRITE error messages as what is wrong, then what is expected
  (`the program is empty: name one`); passing an error up, add what was being done where the
  error doesn't say it, as `stdx::Context` does
