//! What stops a run short of its outcome: a signal that the process notes ([`signal`]), which
//! [`Run::execute`](super::Run::execute) heeds, or a failure; either way, how the run
//! [`Stopped`].

use std::fmt;

/// A signal that interrupts a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Int,
    Term,
    Hup,
}

impl Signal {
    /// Every signal that interrupts a run.
    const ALL: [Self; 3] = [Self::Int, Self::Term, Self::Hup];

    /// The signal that `name` names, as [`Signal::name`] gives it, if one does.
    pub(crate) fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|signal| signal.name() == name)
    }

    /// Its name, as `SIGINT`, as a run's `outcome.json` records it.
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

/// Fails with the interruption, once a signal has come ([`signal`]).
pub(super) fn check() -> Result<(), Stopped> {
    signal().map_or(Ok(()), |signal| Err(Stopped::Interrupted(signal)))
}

/// Why a run stopped without an outcome, as its `outcome.json` records it.
#[derive(Debug, PartialEq, Eq)]
pub enum Stopped {
    /// It failed, for this reason.
    Failed(String),
    /// This signal interrupted it, as [`stdx::signal::caught`] noted it.
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
