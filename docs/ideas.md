# Ideas

Ideas that aren't built. An idea goes away when a change builds it.

## Providers outside the binary

A user should be able to add a provider without a change to this repository.

- A program run once per call: a verb and a JSON request on stdin, a JSON reply on stdout, as Git's
  credential helpers and CNI plugins.
- Its contract: `Provider`'s calls as a versioned wire format in `schema`; the configuration names
  the program.
- Long-lived and framed, as Terraform's plugins, only if the per-call start-up cost matters.
- Blocked by `Provider` settling first, since each later change is a protocol version.

## A schema without I/O

The `#![no_std]` decision core still reaches `schema`'s I/O items.

- `tree::Writer`, `frame` and the `PathBuf` paths move to a crate beside `schema`, which then builds
  as `#![no_std]`.
- Blocked by: the host side's I/O model; an async host beside a synchronous `sandboxd` splits the
  helpers that crate would hold.
