//! Pumpkin's v0 schemas: Rust types serialized as JSON, kept in one place so that an IDL can
//! be adopted later. v0 is unstable.
//!
//! It also holds the wire format of the [sandbox protocol](sandbox), which both of its ends
//! share, with how a sandbox meets the gateway. The crate depends on `serde` and `serde_json`
//! alone, because `sandboxd`, a small static binary, depends on it.
//!
//! Another program's format, such as `docker info`'s, stays with the code that reads it, never
//! here. How Pumpkin words a size or a count is here ([`words`]), as placing's refusals say them.

pub mod ec2_boot;
pub mod frame;
pub mod ir;
pub mod sandbox;
pub mod words;
pub mod workflow;
