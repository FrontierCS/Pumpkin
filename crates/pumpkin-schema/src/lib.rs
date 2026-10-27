//! Pumpkin's v0 schemas: Rust types serialized as JSON, kept in one place so that an IDL can
//! be adopted later. v0 is unstable.
//!
//! The crate depends on serde alone, because `sandboxd`, a small static binary, depends on it.

pub mod ir;
pub mod workflow;
