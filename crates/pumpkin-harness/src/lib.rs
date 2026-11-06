//! The harnesses Pumpkin's agents run, such as Codex ([`codex`]): each is pinned ([`Pin`]), and
//! added to a task's agent image in a layer of its own ([`Pin::recipe`]), so that no task's
//! recipe names a harness.

pub mod codex;
mod pin;

pub use pin::{Mode, Pin, PinnedFile};
