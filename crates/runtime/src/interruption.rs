//! What stops a run short of its outcome: a signal that the process notes, or a failure.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A signal that interrupts a run, by its name, as a run's `outcome.json` records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Signal {
    #[serde(rename = "SIGINT")]
    Int,
    #[serde(rename = "SIGTERM")]
    Term,
    #[serde(rename = "SIGHUP")]
    Hup,
}

impl Signal {
    /// Its name, as `SIGINT`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Int => "SIGINT",
            Self::Term => "SIGTERM",
            Self::Hup => "SIGHUP",
        }
    }
}

/// The signal that interrupts a run, if one has: the first of SIGINT, SIGTERM and SIGHUP that
/// [`stdx::signal`] noted, once the process has it catch them ([`stdx::signal::catch`]).
pub(super) fn signal() -> Option<Signal> {
    let signal = stdx::signal::caught()?;
    let interrupting = match signal {
        rustix::process::Signal::INT => Signal::Int,
        rustix::process::Signal::TERM => Signal::Term,
        rustix::process::Signal::HUP => Signal::Hup,
        // Another signal the process catches is no run's to heed.
        _ => return None,
    };
    Some(interrupting)
}

pub(super) fn check() -> Result<(), Stopped> {
    signal().map_or(Ok(()), |signal| Err(Stopped::Interrupted(signal)))
}

/// Why a run stopped without an outcome, as its `outcome.json` records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stopped {
    Failed(String),
    Interrupted(Signal),
}

impl From<String> for Stopped {
    fn from(reason: String) -> Self {
        Self::Failed(reason)
    }
}

/// How the run stopped, as words that follow its id: `failed: <reason>`, or `was interrupted by
/// <signal>`.
impl fmt::Display for Stopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed(reason) => write!(f, "failed: {reason}"),
            Self::Interrupted(signal) => write!(f, "was interrupted by {}", signal.name()),
        }
    }
}
