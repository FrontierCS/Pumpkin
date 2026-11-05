//! The values that a run and its providers exchange.
//!
//! A provider declares what holds once it is reached ([`Ready`]), creates the sandboxes a run
//! asks for ([`Create`]), and says what it made of them. Every sandbox of a run carries the
//! run's [`RunTag`], by which a provider lists the run's sandboxes, whatever made them.

use std::num::NonZeroU64;
use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;

use crate::ir::{Image, Name, Resources, User};

/// What a provider declares once reached, which holds as long as it is.
#[derive(Debug, Serialize)]
pub struct Ready {
    /// The provider's report of its environment, for the run's record, in a form of its own.
    pub report: Value,
    pub capabilities: Capabilities,
}

/// What a run may rely on of a provider's sandboxes.
#[derive(Debug, Serialize)]
pub struct Capabilities {
    /// The most seconds a sandbox's start takes, from the start of its create until its
    /// `sandboxd` has dialed and its session is set up.
    pub startup_seconds: NonZeroU64,
}

/// The tag that every sandbox of a run carries: an ASCII letter or digit, then any of ASCII
/// letters, digits, `_`, `.` and `-`.
#[derive(Clone, Debug)]
pub struct RunTag(String);

impl RunTag {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RunTag {
    type Error = String;

    fn try_from(tag: String) -> Result<Self, Self::Error> {
        // A provider finds a run's sandboxes by the tag, through command lines that may trim or
        // split what they are given, as `docker` trims a filter's value: these pass unchanged,
        // and a tag that starts with a letter or digit can't be read as an option.
        let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-');
        let expected = "use only ASCII letters, digits, `_`, `.` and `-`";
        let Some(first) = tag.chars().next() else {
            return Err(format!("the run tag is empty: {expected}, at least one"));
        };
        if let Some(c) = tag.chars().find(|&c| !allowed(c)) {
            let (tag, c) = (tag.escape_debug(), c.escape_debug());
            return Err(format!("the run tag `{tag}` holds `{c}`: {expected}"));
        }
        if !first.is_ascii_alphanumeric() {
            return Err(format!(
                "the run tag `{tag}` starts with `{first}`: start it with an ASCII letter or digit"
            ));
        }
        Ok(Self(tag))
    }
}

/// A sandbox to create for a role, whose first process is `sandboxd`, dialing
/// [`dial`](Self::dial), under the [sandbox's rules](crate::sandbox).
#[derive(Debug)]
pub struct Create {
    /// The run the sandbox belongs to, which tags it.
    pub run: RunTag,
    pub role: Role,
    /// The role's image, as this provider's publish gave it.
    pub image: Published,
    /// The static `sandboxd`, as this provider's publish gave it.
    pub sandboxd: Published,
    /// Whether the role's commands make ops: if so, the sandbox has `sandboxctl` at
    /// [`SANDBOXCTL_PATH`](crate::sandbox::SANDBOXCTL_PATH) and its `sandboxd` serves ops; if
    /// not, it is [passive](crate::sandbox).
    pub ops: bool,
    pub dial: Dial,
}

/// A role as a provider sees it: its name, which the provider labels its sandboxes with, its
/// user, which every process in them runs as, and its resources.
#[derive(Debug)]
pub struct Role {
    pub name: Name,
    pub user: User,
    pub resources: Resources,
}

/// What a run's sandboxes run that this host has, which a provider publishes.
#[derive(Debug)]
pub enum Local {
    /// A role's image, as the workflow names it.
    Role { image: Image },
    /// The static `sandboxd` binary, which a role's user can execute.
    Sandboxd { path: PathBuf },
}

/// What a provider's publish made of a [`Local`], for its creates to name: a reference that
/// only that provider reads, such as an image's digest in its registry.
#[derive(Clone, Debug)]
pub struct Published(String);

impl Published {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for Published {
    fn from(reference: String) -> Self {
        Self(reference)
    }
}

/// What a sandbox's `sandboxd` dials to reach the gateway, which accepts it there.
#[derive(Debug)]
pub enum Dial {
    /// The Unix socket at [`SOCKET_NAME`] in `dir`, a directory that the provider shows this
    /// sandbox alone. Its parent must be private to the gateway's user, since the socket takes
    /// the first connection, whoever makes it; the role's user needs to traverse only `dir`.
    /// Unix socket paths are short, so keep it short too. `sandboxd` dials once, as it starts, so
    /// the gateway listens there before the create.
    Socket { dir: PathBuf },
}

/// The name of a [`Dial::Socket`]'s socket in its directory.
pub const SOCKET_NAME: &str = "s";

/// The sandbox a create made.
#[derive(Debug)]
pub struct Created {
    pub id: SandboxId,
}

/// The id a provider gave a sandbox, such as a container's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxId(String);

impl SandboxId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for SandboxId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

/// A sandbox that its run's list finds.
#[derive(Debug)]
pub struct Listed {
    pub id: SandboxId,
}

/// The end of what a sandbox printed as it ran, as its provider keeps it, for diagnostics
/// alone, such as why its `sandboxd` didn't dial.
#[derive(Debug)]
pub struct Console {
    pub text: String,
}
