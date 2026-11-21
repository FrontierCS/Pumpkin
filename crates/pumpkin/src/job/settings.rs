//! What a job takes of a controller's settings across the envelopes that make them: the version
//! of the envelope that made them, their `envelope = "M.m"`, the keys that this build doesn't
//! know, and whether the controller drains, their `draining = true` of envelope 1.0, which refuses
//! each job that starts while it does.
//!
//! A minor adds only keys that a release of an older minor may safely ignore. A key that every
//! release must honour needs a major. So does any removal or change of what a release relies on. A
//! job takes settings of its own major alone, of `OLDEST`'s minor or a later one. In settings of
//! `NEWEST`'s minor or an older one, it refuses a key that it doesn't know, as a typo. In settings
//! of a later minor, it ignores such a key and names it in its log. Settings without `envelope` are
//! of 1.0, as every envelope was before settings named theirs.

use std::fmt;

use crate::{config, say};

/// A version of the envelope, of the major that this build takes.
#[derive(Clone, Copy)]
pub(super) struct Version {
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

// The oldest envelope that this build takes. It takes any later minor of its major too.
const OLDEST: Version = Version { major: 1, minor: 0 };

// The newest envelope whose keys this build knows.
const NEWEST: Version = Version { major: 1, minor: 0 };

const _: () = assert!(OLDEST.major == NEWEST.major && !OLDEST.later(NEWEST));

// The version of settings that name none.
const UNNAMED: Version = Version { major: 1, minor: 0 };

/// The version of the envelope that made `settings`, a controller's settings named as `named`,
/// removed from them, with `draining`. Fails if the version isn't in the form `M.m`, or this build
/// doesn't take it, if `draining` isn't a boolean, or if it is `true`.
pub(super) fn take(settings: &mut toml::Table, named: &str) -> Result<Version, String> {
    let version = envelope(settings, named)?;
    let draining = match settings.remove("draining") {
        None => false,
        Some(value) => value.as_bool().ok_or_else(|| {
            format!("{named} hold `draining = {value}`: expected `true` or `false`")
        })?,
    };
    // A drain takes no new job and leaves those that run to end, as Buildkite's agent stops
    // gracefully:
    // https://github.com/buildkite/agent/blob/13f7c6aba3164116caab64a84332676403d724a4/agent/agent_worker.go#L424-L428
    if draining {
        return Err(format!(
            "{named} hold `draining = true`: the controller's admin is draining it for a \
             cutover, so it starts no job while the jobs it runs end: run this one again once \
             the admin has set `draining = false`"
        ));
    }
    Ok(version)
}

fn envelope(settings: &mut toml::Table, named: &str) -> Result<Version, String> {
    let version = match settings.remove("envelope") {
        None => UNNAMED,
        Some(value) => value.as_str().and_then(parse).ok_or_else(|| {
            format!(
                "{named} hold `envelope = {value}`, which is no envelope's version: expected one \
                 as \"M.m\", such as \"{OLDEST}\""
            )
        })?,
    };
    let Version { major, minor: _ } = OLDEST;
    // A major removes or changes what an older release relies on. So a release refuses each major
    // but its own. Kubernetes likewise removes an API's elements only in a new version of its
    // group:
    // https://github.com/kubernetes/website/blob/b2fed83accab4d0a607783d21740e7de43551369/content/en/docs/reference/deprecation-policy.md#L53-L59
    if version.major > major {
        return Err(format!(
            "{named} are of envelope {version}, a major that this release, of envelope {major}.x \
             from {OLDEST}, doesn't know: run a release that takes {}.x, raising \
             `.pumpkinversion` as the controller's admin says",
            version.major
        ));
    }
    if OLDEST.later(version) {
        return Err(format!(
            "{named} are of envelope {version}, older than this release takes, {major}.x from \
             {OLDEST}: ask the controller's admin to apply an envelope that it takes"
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
