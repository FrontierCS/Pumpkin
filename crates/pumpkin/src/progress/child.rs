//! The words for a run's children: the line of a call, and of a lease with the last
//! command or transfer it ran.

use std::fmt::Write as _;
use std::time::Duration;

use runtime::{CallEnd, CallSpan, Closed, Direction, LeaseSpan, Work};
use schema::sandbox::ExitStatus;
use schema::sandbox::op::{CommandEnd, TransferEnd};

use super::{placed, result, span};

pub(super) fn call_name(call: &CallSpan) -> String {
    match &call.function {
        Some(function) => format!("{function} job {}", call.job),
        None => format!("final judge job {}", call.job),
    }
}

/// The line of a call; the paths of what it kept are shown below `dir`, the record's.
pub(crate) fn called(call: &CallSpan, dir: &std::path::Path) -> String {
    let Some(at) = call.sandbox.gone else {
        return format!(
            "{} on {}, which never ended",
            call_name(call),
            placed(&call.sandbox)
        );
    };
    let asked = call.sandbox.asked;
    let took = match call.sandbox.started {
        Some(started) => format!(
            "started after {}, ran {}",
            span(started.saturating_sub(asked)),
            span(at.saturating_sub(started))
        ),
        None => format!(
            "never started, ended after {}",
            span(at.saturating_sub(asked))
        ),
    };
    let mut line = format!("{} on {}, {took}: ", call_name(call), placed(&call.sandbox));
    let Some(end) = &call.end else {
        line.push_str(STOPPED);
        return line;
    };
    let gave = match end {
        CallEnd::Result(values) => {
            line.push_str(&result(values));
            true
        }
        CallEnd::Invalid(reason) => {
            let _ = write!(line, "an invalid result: {}", crate::escaped(reason));
            false
        }
        CallEnd::Failed(reason) => {
            let _ = write!(line, "failed: {}", crate::escaped(reason));
            false
        }
    };
    for (said, what) in [(call.stopped, "stopped"), (call.timed_out, "timed out")] {
        if said {
            let _ = write!(line, " ({what})");
        }
    }
    let below = |path: &std::path::Path| {
        let path = path.strip_prefix(dir).unwrap_or(path);
        path.display().to_string()
    };
    let mut kept = Vec::new();
    if let Some(stderr) = call.stderr.as_deref().filter(|_| !gave) {
        kept.push(format!("standard error {}", below(stderr)));
    }
    if let Some(evidence) = call.evidence.as_deref().filter(|_| !gave) {
        kept.push(format!("evidence {}", below(evidence)));
    }
    if !kept.is_empty() {
        let _ = write!(line, "; its {}", kept.join(", its "));
    }
    line
}

/// The line of a lease; `now` is a time since the run started, by which an open one is charged.
pub(crate) fn leased(lease: &LeaseSpan, now: Duration) -> String {
    let on = format!("{} on {}", lease_name(lease), placed(&lease.sandbox));
    let Some(gone) = lease.sandbox.gone else {
        return format!("{on}, which never ended");
    };
    let commands = usize::try_from(lease.commands).unwrap_or(usize::MAX);
    let mut line = format!(
        "{on}, {}: charged {}, {} of it busy, {}",
        span(gone.saturating_sub(lease.sandbox.asked)),
        span(lease.charged(now)),
        span(lease.busy(now)),
        crate::counted(commands, "command")
    );
    if let Some(last) = &lease.last {
        let _ = write!(line, ", the last {}", command(last));
    }
    let _ = write!(line, "; {}", lease_end(lease));
    line
}

pub(super) fn lease_name(lease: &LeaseSpan) -> String {
    let name = format!("{} lease {}", lease.grant, lease.lease);
    match lease.holder {
        Some(job) => format!("{name} of job {job}"),
        None => name,
    }
}

/// How a sandbox that the core never ended ended: deleted as the run stopped short of the core's
/// end.
pub(super) const STOPPED: &str = "deleted as the run stopped";

pub(super) fn lease_end(lease: &LeaseSpan) -> String {
    match (&lease.failed, lease.closed) {
        (Some(reason), _) => format!("failed: {}", crate::escaped(reason)),
        (None, Some((_, Closed::Asked))) => "closed by its holder".to_owned(),
        (None, Some((_, Closed::WorkEnded))) => "closed as the work phase ended".to_owned(),
        (None, Some((_, Closed::CallEnded))) => "closed as its call ended".to_owned(),
        (None, Some((_, Closed::Expired))) => "expired".to_owned(),
        (None, None) if lease.sandbox.gone.is_some() => STOPPED.to_owned(),
        (None, None) => "open".to_owned(),
    }
}

pub(super) fn command(work: &Work) -> String {
    let shown = |text: &str, chars| clipped(&crate::escaped(text), chars);
    let (what, ended) = match work {
        Work::Command { argv, end } => {
            let ended = end.map(|end| match end {
                CommandEnd::TimedOut => "timed out".to_owned(),
                CommandEnd::Unstarted => "didn't start".to_owned(),
                CommandEnd::Exited(status) => exited(status),
            });
            let what = format!("`{}`", shown(&argv.join(" "), COMMAND_SHOWN_CHARS));
            (what, ended)
        }
        Work::Transfer {
            direction,
            from,
            to,
            end,
        } => {
            let (from, to) = (shown(from, PATH_SHOWN_CHARS), shown(to, PATH_SHOWN_CHARS));
            let what = match direction {
                Direction::Put => format!("put {from} to {to}"),
                Direction::Replace => format!("put {from} to {to}, replacing it"),
                Direction::Get => format!("get {from} to {to}"),
            };
            let ended = end.as_ref().map(|end| match end {
                TransferEnd::Moved { entries, bytes } => format!(
                    "moved {}, {}",
                    crate::counted(*entries, "entry"),
                    crate::counted(*bytes, "byte")
                ),
                TransferEnd::Failed(reason) => format!("failed: {}", crate::escaped(reason)),
            });
            (what, ended)
        }
    };
    format!("{what} {}", ended.as_deref().unwrap_or("running"))
}

/// The most characters of a lease's command that `ls` and a summary show.
const COMMAND_SHOWN_CHARS: usize = 60;

/// The most characters of each path of a lease's transfer that `ls` and a summary show.
const PATH_SHOWN_CHARS: usize = 40;

/// `text`, cut after `chars`, as `…` ends it then.
fn clipped(text: &str, chars: usize) -> String {
    match text.char_indices().nth(chars) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

pub(super) fn exited(status: ExitStatus) -> String {
    match status {
        ExitStatus::Code(code) => format!("exited {code}"),
        ExitStatus::Signal(signal) => format!("ended by signal {signal}"),
    }
}
