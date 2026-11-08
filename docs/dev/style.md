# Style

The rules for writing Rust in Pumpkin: types, interfaces, control flow, lints and errors.

- PREFER types that only hold valid values: parse input once at the boundary, and take the
  parsed types everywhere past it
- PREFER interfaces (flags, configuration, protocol messages, `pub` APIs) that express exactly
  what real runs need: a value fixed in every real run is a constant, not a parameter, and each
  setting has one way to set it
- PREFER flags for per-run choices, `pumpkin.toml` for per-machine facts, and the environment
  only for secrets that must stay out of `argv`
- PREFER moving branches up to the caller and loops down into functions that take a batch
- PREFER exhaustive `match` without wildcard arms, and destructuring structs in full when
  converting them
- PREFER `#[expect(lint, reason = "...")]` over `#[allow(lint)]`
- PREFER a type that makes a bug-only path unrepresentable; where no type can express it, or one
  would cost more than it saves (such as a reshaped `pub` API or a dependency), WRITE
  `unreachable!` under `#[expect(clippy::unreachable, reason = "…")]` saying why only a bug can
  reach it; never handle such a path. Handle, or return, every error the world can cause, the
  kernel's and its security policy's included
- WRITE an error type of its own only for the cases a caller branches on, in this process,
  across a protocol or in a record (an enum, a nested `Result`, a reply's code), or for a check
  that reports every problem it finds in an input, each with where it is (`ParseErrors`); any
  other error is a message, in whichever of `String`, `io::Error` or `Box<dyn Error>` needs the
  fewest conversions, and code never branches on the text of a message Pumpkin wrote
- WRITE error messages as what is wrong, then what is expected
  (`the program is empty: name one`); passing an error up, add what was being done where the
  error doesn't say it, as `stdx::Context` does
