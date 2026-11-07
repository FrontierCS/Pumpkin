//! Pumpkin's v0 schemas: Rust types serialized as JSON, kept in one place so that an IDL can
//! be adopted later. v0 is unstable.
//!
//! It also holds the wire format of the [sandbox protocol](sandbox), which both of its ends
//! share, and the values of the [provider interface](provider). The crate depends on `serde` and
//! `serde_json` alone, because `sandboxd`, a small static binary, depends on it.

pub mod ir;
pub mod provider;
pub mod sandbox;
pub mod workflow;
