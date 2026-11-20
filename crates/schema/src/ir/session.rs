//! Sessions, the kind of function that leases runners for its holder to drive.

use std::num::{NonZeroU32, NonZeroU64};

use serde::{Deserialize, Serialize};

use super::Name;

/// A session that a target holds, written as an agent's grant of one is:
/// `{"name": "box", "session": {…}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedSession {
    /// Unique among the target's sessions.
    pub name: Name,
    pub session: Session,
}

/// The host's six functions over leases of `runner`, each lease a fresh instance of the role that
/// runs the commands its holder sends, called as `<name>.open`, `.run`, `.put`, `.get`, `.status`
/// and `.close`.
///
/// A lease runs one command at a time, until the holder closes it, its time runs out, or the
/// holder's work ends: the work phase, for the agent; its body, for a target's call. Its time runs
/// from when its runner is ready until it ends. A target's limits are each call's: a call starts
/// with all of them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    /// The runner role that each lease starts afresh. Distinct agents may lease the same runner
    /// role. A target's session names a runner no other session names, and an agent's runner
    /// has an image no target has.
    pub runner: Name,
    /// The most leases open at once, each until its runner is gone.
    pub instances: NonZeroU32,
    /// The most time each lease may last.
    pub instance_seconds: NonZeroU64,
    /// The most time the session's leases may last in all, each lease open at once counting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cumulative_seconds: Option<NonZeroU64>,
    /// The most commands the session's leases may run in all, each transfer of a tree to or from
    /// a lease's runner counting as one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commands: Option<NonZeroU64>,
    /// The most bytes of files each transfer may move, at most
    /// [`MAX_TRANSFER_BYTES`](crate::sandbox::MAX_TRANSFER_BYTES): a tree that holds more is
    /// refused as it is collected.
    pub transfer_bytes: NonZeroU64,
}
