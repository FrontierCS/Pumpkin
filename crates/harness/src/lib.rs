//! The harnesses Pumpkin's agents run, such as Codex ([`codex`]): each is pinned, and added to a
//! task's agent image in a layer of its own, so that no task's recipe names a harness, and each
//! writes the agent's runtime that runs it.

// Without `std`, the crate can name no file, network, process, environment, thread or clock, so
// that what an adapter writes depends on its arguments alone, though `schema`'s I/O items stay
// reachable through it; `alloc` gives it what else it uses, such as `Vec` and `String`.
#![no_std]

extern crate alloc;

pub mod codex;
mod pin;

pub use pin::{BASE, Mode, Pin, PinnedFile};
