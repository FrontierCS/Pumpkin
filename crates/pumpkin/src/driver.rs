//! The driver runs a workflow: it decides, the provider and the sessions act.
//!
//! The decisions are in [`core`], which is pure: it takes what happened as events and returns
//! what to do next as actions. A runtime around it observes the sandboxes, feeds the core,
//! and performs its actions.

pub mod core;
