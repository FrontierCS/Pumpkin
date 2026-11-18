//! The values that a run and its providers exchange through [`Provider`](crate::Provider).

use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU32;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use schema::ir::{Name, Resources, User};
use schema::workflow::Class;
use serde::Serialize;
use serde_json::Value;

/// What a provider declares when a run connects to it, which holds while the run is connected.
#[derive(Debug, Serialize)]
pub struct Ready {
    /// What the run's record keeps of the provider, in a form of its own: what a reap needs to
    /// reach it again beside the configuration that ran the run, and each string that names its
    /// account, which an export replaces; `null` if neither.
    pub report: Value,
    pub classes: BTreeMap<Name, Class>,
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

/// A sandbox to create for a role, whose supervisor is `sandboxd`, meeting the gateway as its
/// provider's [`Meeting`](crate::Meeting) says, under the [sandbox's rules](schema::sandbox).
#[derive(Debug)]
pub struct Create {
    pub run: RunTag,
    /// The sandbox's name, which no other sandbox of its run has, though another run's may: a
    /// key a provider may know it by on its platform, beside the id it gives it.
    pub name: Name,
    pub role: Role,
    /// Which of its role's sandboxes at once it is, from 0, below the role's
    /// [`instances`](Role::instances): no two of a run's sandboxes of one role that run at once
    /// are the same instance, so a provider that shares a host gives each its own share.
    pub instance: u32,
    /// The class of sandbox to create, by its name among the provider's
    /// ([`Ready::classes`]), which the role fits.
    pub class: Name,
    pub image: ImageRef,
    pub sandboxd: SandboxdRef,
    /// Whether the role's commands make ops: if so, the sandbox has `sandboxctl` at
    /// [`SANDBOXCTL_PATH`](schema::sandbox::SANDBOXCTL_PATH) and its `sandboxd` serves ops; if
    /// not, it is [passive](schema::sandbox).
    pub ops: bool,
    /// Whether the sandbox's `sandboxd` serves the model endpoint, at
    /// [`MODEL_PORT`](schema::sandbox::MODEL_PORT).
    pub model: bool,
    /// The run's hard end, by this host's clock, which bounds how long the sandbox runs should
    /// the run's driver die ([`Creator::create`](crate::Creator::create)).
    pub hard_end: UnixTime,
    /// When the run stops waiting for the sandbox to start, by this host's clock, at most the
    /// hard end: the create's deadline ([`Creator::create`](crate::Creator::create)).
    pub by: UnixTime,
}

/// A time by a machine's clock, as the time since the Unix epoch, never before it: apart from
/// the times since a run's start, which are [`Duration`]s.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnixTime(Duration);

impl UnixTime {
    /// `time`, a time by the machine's clock; fails if it is before the Unix epoch.
    pub fn of(time: SystemTime) -> Result<Self, String> {
        let since = time.duration_since(UNIX_EPOCH);
        let since = since.map_err(|_| "the clock reads before 1970-01-01: set the system clock");
        Ok(Self(since?))
    }

    /// The time `since` after the Unix epoch.
    pub const fn after_epoch(since: Duration) -> Self {
        Self(since)
    }

    pub const fn since_epoch(self) -> Duration {
        self.0
    }

    /// How long after `earlier` it is, or zero if it is before `earlier`, as by a clock set back.
    pub const fn since(self, earlier: Self) -> Duration {
        self.0.saturating_sub(earlier.0)
    }

    /// The time `duration` after it, or the latest one if that is past what a `Duration` holds.
    #[must_use]
    pub const fn saturating_add(self, duration: Duration) -> Self {
        Self(self.0.saturating_add(duration))
    }
}

/// A role as a provider sees it: its name, which the provider labels its sandboxes with, its
/// user, which every process in them runs as, and its resources.
#[derive(Debug)]
pub struct Role {
    pub name: Name,
    pub user: User,
    /// Each of its sandboxes'.
    pub resources: Resources,
    /// How many of its sandboxes a run runs at once.
    pub instances: NonZeroU32,
}

/// What a provider's `publish_image` made of a role's image, for the role's creates to name: a
/// reference that only that provider reads, such as the image's digest in its registry.
#[derive(Clone, Debug)]
pub struct ImageRef(String);

impl ImageRef {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for ImageRef {
    fn from(reference: String) -> Self {
        Self(reference)
    }
}

/// What a provider's `publish_sandboxd` made of the static `sandboxd`, for every create to name:
/// a reference that only that provider reads, such as where it keeps a copy.
#[derive(Clone, Debug)]
pub struct SandboxdRef(String);

impl SandboxdRef {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for SandboxdRef {
    fn from(reference: String) -> Self {
        Self(reference)
    }
}

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

/// The sandboxes that a delete failed to delete ([`Sweep::delete`](crate::Sweep::delete)), at
/// least one, each with why.
#[derive(Debug)]
pub struct Undeleted(Vec<(SandboxId, String)>);

impl Undeleted {
    /// What a delete that failed each of `failed` returns: `Ok` if it failed none.
    pub fn of(failed: Vec<(SandboxId, String)>) -> Result<(), Self> {
        if failed.is_empty() {
            Ok(())
        } else {
            Err(Self(failed))
        }
    }

    /// Why the delete failed `id`, if it did.
    pub fn why(&self, id: &SandboxId) -> Option<&str> {
        let failed = self.0.iter().find(|(failed, _)| failed == id);
        failed.map(|(_, why)| why.as_str())
    }
}

/// Each why once, as a failure of several ids at once is the why of each.
impl fmt::Display for Undeleted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut said: Vec<&str> = Vec::new();
        for (_, why) in &self.0 {
            if !said.contains(&why.as_str()) {
                said.push(why);
            }
        }
        f.write_str(&said.join("; "))
    }
}

#[derive(Debug)]
pub struct Listed {
    pub id: SandboxId,
    /// Whether the sandbox has ended: its `sandboxd` has exited, or its provider has stopped
    /// it, so it will never dial or serve a session again.
    pub ended: bool,
}

/// The end of what a sandbox printed as it ran, as its provider keeps it, for diagnostics
/// alone, such as why its `sandboxd` didn't dial.
#[derive(Debug)]
pub struct Console {
    pub text: String,
}
