# Style

The rules for writing Rust in Pumpkin: types, naming, interfaces, bounds, control flow,
assertions, lints and errors.

- PREFER types that only hold valid values: parse input once at the boundary, and take the
  parsed types everywhere past it
- NAME a number by what it measures, its unit last (`payload_bytes`, `calls_count`): an `index`
  counts from 0, a `count` from 1 and a `size` in bytes, and a time past the boundary is a
  `Duration`
- PREFER interfaces (flags, configuration, protocol messages, `pub` APIs) that express exactly
  what real runs need: a value fixed in every real run is a constant, not a parameter, and each
  setting has one way to set it
- PREFER flags for per-run choices, `pumpkin.toml` for per-machine facts, and the environment
  only for where the system keeps things, such as `HOME`, and for secrets that must stay out of
  `argv`
- FOLLOW [Tiger Style](https://github.com/tigerbeetle/tigerbeetle/blob/68958eb757747d0fc96a79d1f9c2fa6766b67b48/docs/TIGER_STYLE.md)
  in code on a `stdx::io` loop: a callback runs only in the loop's batch and goes last in its
  signature, the operations in flight have a fixed limit, and each deadline is a time on the
  loop's clock; allocate statically only what has a fixed size, such as the slab of operations
- PREFER moving branches up to the caller and loops down into functions that take a batch
- KEEP each file to one thing: put a new thing in a module of its own, and split a file that
  holds several into modules that each hold one, in a pull request that only moves code;
  `cargo xtask ci` caps a source file's lines
- BOUND whatever input or time can grow, each queue, buffer, read, retry and recursion, by a
  limit from precedent or the protocol, named as a constant at its home; a loop meant never to
  end returns `!`
- PREFER exhaustive `match` without wildcard arms, and destructuring structs in full when
  converting them
- PREFER `#[expect(lint, reason = "...")]` over `#[allow(lint)]`
- PREFER a type that makes a bug-only path unrepresentable; where no type can express it, or one
  would cost more than it saves (such as a reshaped `pub` API or a dependency), WRITE
  `unreachable!("…")` with a message saying why only a bug can reach it; never handle such a
  path. Handle, or return, every error the world can cause, the kernel's and its security
  policy's included
- WRITE `assert!` for an invariant that no type can hold, such as a count within its limit or
  one event after another, most of all in code a simulation test drives, and
  `const _: () = assert!(…)` for a relation between constants that code relies on; never
  `debug_assert!`: a check that release builds skip guards nothing in a real run
- WRITE an error type of its own only for the cases a caller branches on, in this process,
  across a protocol or in a record (an enum, a nested `Result`, a reply's code), or for a check
  that reports every problem it finds in an input, each with where it is (`ParseErrors`); any
  other error is a message, in whichever of `String`, `io::Error` or `Box<dyn Error>` needs the
  fewest conversions, and code never branches on the text of a message Pumpkin wrote
- WRITE error messages as what is wrong, then what is expected
  (`the program is empty: name one`); passing an error up, add what was being done where the
  error doesn't say it, as `stdx::Context` does
