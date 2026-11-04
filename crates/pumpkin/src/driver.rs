//! The driver, which decides what a run of a workflow does.
//!
//! The decisions are in [`core`], which is pure: it takes what happened as events and returns
//! what to do next as actions. [`core::Core`] says how a runtime drives it.

pub mod core;
