//! What a job takes of a controller's settings across the envelopes that make them. The settings
//! name the version of the envelope that made them, as `envelope = "M.m"`. Their `[deployment]` is
//! the host's ([`deployment`]), and a job leaves it out. They may hold keys that this build doesn't
//! know. They may say `draining = true`, which refuses each job that starts while they do.
//!
//! A minor adds only keys that a release of an older minor may safely ignore. A key that every
//! release must honour needs a major. So does any removal or change of what a release relies on. A
//! release takes settings of its own major alone, of `OLDEST`'s minor or a later one. In settings
//! of `NEWEST`'s minor or an older one, a job refuses a key that it doesn't know, as a typo. In
//! settings of a later minor, it ignores such a key and names it in its log.

use std::fmt;

use aws::ParameterArn;
use serde::Deserialize;

use crate::controller::Name;
use crate::{config, say};

/// A version of the envelope, of the major that this build takes.
#[derive(Clone, Copy)]
pub(crate) struct Version {
    major: u32,
    minor: u32,
}

impl Version {
    const fn later(self, than: Self) -> bool {
        self.major > than.major || (self.major == than.major && self.minor > than.minor)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// The oldest envelope that this build takes. It takes any later minor of its major too.
pub(super) const OLDEST: Version = Version { major: 1, minor: 1 };

// The newest envelope whose keys this build knows: 1.1 adds `[deployment]`.
const NEWEST: Version = Version { major: 1, minor: 1 };

const _: () = assert!(OLDEST.major == NEWEST.major && !OLDEST.later(NEWEST));

const DEPLOYMENT: &str = "deployment";

/// The `[deployment]` of a controller's settings, which the deployment wrote.
#[derive(Deserialize)]
pub(crate) struct Deployment {
    /// The `name` that the deployment was applied with.
    pub(crate) name: Name,
    /// The SSM parameter of the members' keys.
    pub(crate) members: ParameterArn,
}

/// Parses `value`, the text of a controller's settings named as `named`, as TOML's tables.
pub(crate) fn tables(value: &str, named: &str) -> Result<toml::Table, String> {
    toml::from_str(value)
        .map_err(|error| format!("{named} aren't TOML: {}", error.to_string().trim_end()))
}

/// Removes the version of the envelope and the `[deployment]` from `settings`, a controller's
/// settings named as `named`, and returns both. Fails unless the version is in the form `M.m`, and
/// this build takes it. Fails too if the settings hold no `[deployment]`, or one that lacks a key.
// A key that a later minor adds to the table is ignored, as a job ignores such keys of the
// settings.
pub(crate) fn deployment(
    settings: &mut toml::Table,
    named: &str,
) -> Result<(Version, Deployment), String> {
    let version = envelope(settings, named)?;
    let Some(deployment) = settings.remove(DEPLOYMENT) else {
        return Err(format!(
            "{named} hold no `[deployment]`: the controller's admin applies this release's \
             deploy/aws, which writes it"
        ));
    };
    let deployment = deployment.try_into().map_err(|error: toml::de::Error| {
        let error = error.to_string();
        format!(
            "{named} hold a `[deployment]` that is refused: {}: write its `name` and `members` \
             as deploy/aws does",
            error.trim_end()
        )
    })?;
    Ok((version, deployment))
}

/// What [`take`] removes from a controller's settings.
pub(crate) struct Taken {
    pub(crate) version: Version,
    /// The host's table, which a job reads none of.
    pub(crate) deployment: Deployment,
    /// Whether the settings drain.
    pub(crate) draining: bool,
}

/// Removes the version of the envelope, `[deployment]` and `draining` from `settings`, a
/// controller's settings named as `named`, and returns them. Fails as [`deployment`] does. Fails
/// too if `draining` isn't a boolean.
pub(crate) fn take(settings: &mut toml::Table, named: &str) -> Result<Taken, String> {
    let (version, deployment) = deployment(settings, named)?;
    let draining = match settings.remove("draining") {
        None => false,
        Some(value) => value.as_bool().ok_or_else(|| {
            format!("{named} hold `draining = {value}`: expected `true` or `false`")
        })?,
    };
    Ok(Taken {
        version,
        deployment,
        draining,
    })
}

/// Why a job is refused while `named`, a controller's settings, drain.
// A drain takes no new job and leaves those that run to end. Buildkite's agent stops gracefully
// this way:
// https://github.com/buildkite/agent/blob/13f7c6aba3164116caab64a84332676403d724a4/agent/agent_worker.go#L424-L428
pub(super) fn drained(named: &str) -> String {
    format!(
        "{named} hold `draining = true`: the controller's admin is draining it for a cutover, so \
         it starts no job while the jobs it runs end: run this one again once the admin has set \
         `draining = false`"
    )
}

fn envelope(settings: &mut toml::Table, named: &str) -> Result<Version, String> {
    let Version { major, minor: _ } = OLDEST;
    let Some(value) = settings.remove("envelope") else {
        return Err(format!(
            "{named} name no `envelope`, where this release takes {major}.x from {OLDEST}: the \
             controller's admin applies this release's deploy/aws, which writes it"
        ));
    };
    let version = value.as_str().and_then(parse).ok_or_else(|| {
        format!(
            "{named} hold `envelope = {value}`, which is no envelope's version: expected one as \
             \"M.m\", such as \"{OLDEST}\""
        )
    })?;
    // A major removes or changes what an older release relies on. So a release refuses each major
    // but its own. Kubernetes likewise removes an API's elements only in a new version of its
    // group:
    // https://github.com/kubernetes/website/blob/b2fed83accab4d0a607783d21740e7de43551369/content/en/docs/reference/deprecation-policy.md#L53-L59
    if version.major > major {
        return Err(format!(
            "{named} are of envelope {version}, a major that this release, of envelope {major}.x \
             from {OLDEST}, doesn't know: raise `.pumpkinversion`, or run the `pumpkin-admin`, of \
             a release that takes {}.x",
            version.major
        ));
    }
    if OLDEST.later(version) {
        return Err(format!(
            "{named} are of envelope {version}, older than this release takes, {major}.x from \
             {OLDEST}: the controller's admin applies this release's deploy/aws, which writes \
             {NEWEST}"
        ));
    }
    Ok(version)
}

/// Removes from `tables` the keys that no table of a configuration takes, and names them in the
/// job's log. `tables` is a job's configuration made of `named`, of envelope `version`. Settings of
/// a later minor than `NEWEST` may hold such keys. Fails if settings of `NEWEST` or an older minor
/// hold any. Fails too if `tables` isn't a configuration once they are gone.
pub(super) fn remove_unknown(
    tables: &mut toml::Table,
    named: &str,
    version: Version,
) -> Result<(), String> {
    let unknown =
        config::remove_unknown(tables).map_err(|error| format!("{named} are refused: {error}"))?;
    if unknown.is_empty() {
        return Ok(());
    }
    let keys = unknown.join(", ");
    if !version.later(NEWEST) {
        return Err(format!(
            "{named} hold {keys}, which no table of envelope {version}'s settings takes: correct \
             or remove each"
        ));
    }
    // Once, as Cargo warns of each key of a manifest that it doesn't know:
    // https://github.com/rust-lang/cargo/blob/111443f3dc4d82a65e2306a93028f122406401c1/src/workspace/parser/mod.rs#L3003-L3015
    say(format_args!(
        "{named} are of envelope {version}, a later minor than this release knows, {NEWEST}, and \
         hold {keys}, which it ignores: to take them, raise `.pumpkinversion` to a release that \
         knows {version}"
    ));
    Ok(())
}

fn parse(text: &str) -> Option<Version> {
    let (major, minor) = text.split_once('.')?;
    let number = |digits: &str| {
        // `u32`'s parse would take a sign too.
        let all = !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit());
        all.then(|| digits.parse().ok()).flatten()
    };
    Some(Version {
        major: number(major)?,
        minor: number(minor)?,
    })
}
