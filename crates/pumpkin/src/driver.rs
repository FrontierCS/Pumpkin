//! The driver, which runs a workflow: it decides what the run does, the provider and the
//! sessions act, and the run log records it.
//!
//! The decisions are in [`core`], which is pure: it takes what happened as events and returns
//! what to do next as actions. The runtime, [`run`], drives it as [`core::Core`] says: it
//! observes the sandboxes, feeds the core, performs its actions, and keeps the run's record.

pub mod core;
pub mod run;
