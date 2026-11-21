//! The words for a run's children: the line of a call, and of a lease with the last
//! command or transfer it ran.

use std::fmt::Write as _;
use std::time::Duration;

use runtime::{CallEnd, CallOf, CallSpan, Closed, Direction, LeaseSpan, Work};
use schema::sandbox::ExitStatus;
use schema::sandbox::op::{CommandEnd, TransferEnd};

use super::{placed, result, span};

pub(super) fn call_name(call: &CallSpan) -> String {
    match &call.of {
        CallOf::Function(function) => {
            let mut name = format!("{function} job {}", call.job);
            if let Some(caller) = call.caller {
                let _ = write!(name, " of {}", super::caller_lane(caller));
            }
            name
        }
        CallOf::Final => format!("final call job {}", call.job),
    }
}

/// When `call` ended: as its end was reported, or as its sandbox was gone, for one stopped short of
/// that report.
pub(super) fn ended(call: &CallSpan) -> Option<Duration> {
    (call.end.as_ref().map(|(at, ..)| *at)).or(call.sandbox.gone)
}

/// How `call` ended, as its line and its milestone say it.
pub(super) fn call_end(call: &CallSpan) -> String {
    match &call.end {
        Some((_, CallEnd::Result(values), _)) => result(values),
        Some((_, CallEnd::Failed(reason), _)) => format!("failed: {}", crate::escaped(reason)),
        None => STOPPED.to_owned(),
    }
}

/// The line of a call.
pub(crate) fn called(call: &CallSpan) -> String {
    let Some(at) = ended(call) else {
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
    line.push_str(&call_end(call));
    if call.end.is_none() {
        return line;
    }
    for (said, what) in [(call.stopped, "stopped"), (call.timed_out, "timed out")] {
        if said {
            let _ = write!(line, " ({what})");
        }
    }
    line.push_str(&over_budget(call));
    line
}

/// What ends the line of a call's end if its route barred any of its model requests for a spent
/// model budget, as `; 1 model request barred by judge's model budget`; else nothing.
pub(super) fn over_budget(call: &CallSpan) -> String {
    let Some((_, _, Some(count))) = call.end else {
        return String::new();
    };
    let role = call.role.as_deref().unwrap_or("its role");
    let barred = crate::counted(count.get(), "model request");
    format!("; {barred} barred by {role}'s model budget")
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
    format!("{name} of {}", super::caller_lane(lease.holder))
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
