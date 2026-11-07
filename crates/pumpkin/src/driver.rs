//! The driver, which decides what a run of a workflow does, and keeps the run's record.
//!
//! The decisions are in [`pumpkin_decision`], which is pure: it takes what happened as events
//! and returns what to do next as actions. [`pumpkin_decision::Core`] says how a runtime drives
//! it. The runtime, [`run`], prepares a run, then drives the core to the run's end, keeping its
//! record.

pub mod run;
