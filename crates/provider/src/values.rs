//! The values that a run and its providers exchange through [`Provider`](crate::Provider).

use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU32;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use schema::ir::{AssetPath, CompanionName, Name, Resources, Sha256Digest, User};
use schema::sandbox::{Endpoint, Listening, Relayed};
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
    /// The assets that its `main` holds, each read-only at its path. Only a provider that
    /// publishes assets ([`Assets`](crate::Assets)) is given any.
    pub assets: Vec<HeldAsset>,
    pub sandboxd: SandboxdRef,
    /// Whether the role's commands make ops: if so, the sandbox has `sandboxctl` at
    /// [`SANDBOXCTL_PATH`](schema::sandbox::SANDBOXCTL_PATH) and its `sandboxd` serves ops; if
    /// not, it is [passive](schema::sandbox).
    pub ops: bool,
    /// Whether the sandbox's `sandboxd` serves the model endpoint, at
    /// [`MODEL_PORT`](schema::sandbox::MODEL_PORT).
    pub model: bool,
    /// The sandbox's hard end, by this host's clock: when what it holds is to be collected, at
    /// most the run's hard end. It bounds how long the sandbox runs should the run's driver die
    /// ([`Creator::create`](crate::Creator::create)).
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
    /// How many companions each of its sandboxes holds beside its own container: a runner's, as
    /// it declares them, and none for any other role. [`fits`](crate::Provider::fits) refuses
    /// more than its sandbox holds.
    pub companions: usize,
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

/// The digest that names an asset's files, as [`AssetFiles`](schema::ir::AssetFiles) defines it,
/// by which a provider keeps them.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AssetDigest(Sha256Digest);

impl AssetDigest {
    /// Its 64 hexadecimal digits, without `sha256:`.
    pub fn hex(&self) -> &str {
        self.0.hex()
    }
}

impl From<Sha256Digest> for AssetDigest {
    fn from(digest: Sha256Digest) -> Self {
        Self(digest)
    }
}

impl fmt::Display for AssetDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// What a provider's `publish_asset` made of an asset's files
/// ([`Assets::publish_asset`](crate::Assets::publish_asset)).
#[derive(Debug)]
pub struct PublishedAsset {
    /// For each create whose sandbox holds the files to name ([`HeldAsset::reference`]).
    pub reference: AssetRef,
    /// Whether this publish fetched the files: false if the provider held them already.
    pub fetched: bool,
}

/// A reference to an asset's files that a provider published, which only that provider reads, such
/// as where it keeps them.
#[derive(Clone, Debug)]
pub struct AssetRef(String);

impl AssetRef {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for AssetRef {
    fn from(reference: String) -> Self {
        Self(reference)
    }
}

/// An asset that a sandbox holds read-only at `path`, of the files that its provider published as
/// `reference` ([`Create::assets`]).
#[derive(Clone, Debug)]
pub struct HeldAsset {
    pub path: AssetPath,
    pub reference: AssetRef,
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
    /// The steps of the create that the provider timed, for the run's record alone. A provider
    /// that times none gives none.
    pub phases: Vec<Phase>,
}

/// What the containers of a sandbox that listens are given to meet the gateway by
/// ([`Meeting::Listen`](crate::Meeting::Listen)).
///
/// The create starts a `sandboxd` in each container. `main`'s is given `main`, and each of a
/// runner's companions, in their declared order, is given its own.
#[derive(Debug)]
pub struct ListenDials {
    pub main: Listening,
    pub companions: Vec<CompanionDial>,
}

/// A companion of a runner's sandbox that listens, as the runner declares it
/// ([`ir::Companion`](schema::ir::Companion)) but for its `cwd` and `env`.
///
/// Each command there is given those as it starts. Its image is as the provider published it,
/// and `relayed` is what its `sandboxd` is given.
#[derive(Debug)]
pub struct CompanionDial {
    pub name: CompanionName,
    pub image: ImageRef,
    pub user: User,
    pub relayed: Relayed,
}

/// What a create of a sandbox that listens returns: where the gateway dials `main`, and its relay.
///
/// The relay carries the gateway to each companion ([`Relayed`]). `relay` is some exactly when
/// the create was given companions.
#[derive(Debug)]
pub struct Listened {
    pub created: Created,
    pub main: Endpoint,
    pub relay: Option<Endpoint>,
}

/// A step of a create, timed by the provider's own monotonic clock.
///
/// It lies within the create: it begins no earlier than [`Creator::create`](crate::Creator::create)
/// was called, and ends no later than it returned. A create's phases may overlap, and may leave
/// gaps between them and at either end.
#[derive(Debug)]
pub struct Phase {
    /// What the step did, in the provider's words, such as `lookup`.
    pub name: String,
    /// When the step began, since [`Creator::create`](crate::Creator::create) was called.
    pub began: Duration,
    /// How long the step took, from its start to its end.
    pub took: Duration,
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
