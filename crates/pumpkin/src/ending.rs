//! Following a run until it ends, and saying how it ended: its outcome, if asked for, its
//! summary, and the commands to run next.

use std::path::Path;
use std::time::{Duration, Instant};
use std::{fmt, fs};

use runtime::{BUILD, Ending, Record};
use schema::ir::Name;
use serde_json::Value;
use stdx::Context;

use crate::cli::Exit;
use crate::{advise, next, print, progress, say, to_stderr};

/// What a command that says how a run ended prints on the standard output.
#[derive(Clone, Copy)]
pub(crate) enum Printed {
    /// The run's `outcome.json`, the file's bytes.
    Outcome,
    Nothing,
}

impl Printed {
    /// Prints what `self` asks for of the run whose record is `dir`, which fails if that is an
    /// `outcome.json` the record lacks.
    pub(crate) fn print(self, dir: &Path) -> Result<(), String> {
        match self {
            Self::Outcome => {
                let recorded = dir.join("outcome.json");
                let printed = fs::read(&recorded).context(recorded.display())?;
                print(&printed).context("printing the outcome failed")
            }
            Self::Nothing => Ok(()),
        }
    }
}

/// Says how `record`'s run ended, following it first if it is live, after printing what
/// `printed` asks for. `command` is the command that follows it, which the following names, and
/// `at` is where the commands to run next find it.
pub(crate) fn follow(
    record: &Record,
    command: &str,
    at: &next::At,
    printed: Printed,
) -> Result<Exit, String> {
    let ending = if let Some(ending) = record.ending()? {
        ending
    } else {
        following(record, command);
        followed(record)?
    };
    end(record, &ending, printed, at, record.args())
}

// The first pause is short, to see soon what has just happened, the run's end most of all, and it
// bounds how often a run whose log keeps growing is polled. Each after is twice the last, up to a
// second, which bounds what a long quiet run's follow costs. Go's `http.Server.Shutdown` doubles
// its pauses so while it polls for idle connections, from 1 ms up to 500 ms:
// https://github.com/golang/go/blob/46b62495e1bf4d56e0313320772ae588d9119aed/src/net/http/server.go#L3388-L3395
// https://github.com/golang/go/blob/46b62495e1bf4d56e0313320772ae588d9119aed/src/net/http/server.go#L3431-L3454
const FIRST_FOLLOW_POLL: Duration = Duration::from_millis(10);
const LAST_FOLLOW_POLL: Duration = Duration::from_secs(1);

/// The pauses between a follower's polls.
pub(crate) struct Polls(Duration);

impl Polls {
    pub(crate) const fn new() -> Self {
        Self(FIRST_FOLLOW_POLL)
    }

    /// The pause before the next poll: `FIRST_FOLLOW_POLL` at first and after a restart, then
    /// twice the last, up to `LAST_FOLLOW_POLL`.
    pub(crate) fn pause(&mut self) -> Duration {
        let pause = self.0;
        self.0 = (pause * 2).min(LAST_FOLLOW_POLL);
        pause
    }

    /// Makes the next pause the first again, as more is likely soon after something new.
    // As gRPC resets its connection backoff once a connection has worked:
    // https://github.com/grpc/grpc/blob/785ff9afd6410c535a44690e7cca1f81228d8851/doc/connection-backoff.md#L49-L55
    pub(crate) const fn restart(&mut self) {
        self.0 = FIRST_FOLLOW_POLL;
    }
}

/// Waits until the run of `record`, which was live, has ended, or been lost, and returns how;
/// says each of its milestones meanwhile, once, and the parts of its log that can't be read, if
/// any, to the operator.
pub(crate) fn followed(record: &Record) -> Result<Ending, String> {
    milestones(
        record,
        |message| {
            say(message);
            Ok(())
        },
        |pause| {
            std::thread::sleep(pause);
            Ok(())
        },
    )
}

pub(crate) fn milestones(
    record: &Record,
    mut tell_reader: impl FnMut(&dyn fmt::Display) -> Result<(), String>,
    mut wait: impl FnMut(Duration) -> Result<(), String>,
) -> Result<Ending, String> {
    let mut said = std::collections::BTreeSet::new();
    let mut said_unread = false;
    let mut log = record.fold();
    let mut polls = Polls::new();
    let mut read_out: Option<Instant> = None;
    loop {
        // How it ended first, so that the log read after holds every milestone before the end.
        let ending = record.ending()?;
        let taken = log.taken();
        let started_over = log.take()?;
        if started_over.is_some() || log.taken() != taken {
            polls.restart();
        }
        if let Some(started_over) = started_over {
            tell_reader(&format_args!("run {}: {started_over}", record.id()))?;
        }
        // A readout reads each sandbox's `sandbox.json`, and so costs what the run has done so far:
        // it comes once a second, as a milestone said within is timely, and at the end.
        if ending.is_some() || read_out.is_none_or(|at| at.elapsed() >= LAST_FOLLOW_POLL) {
            read_out = Some(Instant::now());
            let runtime::Readout { progress, unread } = log.readout()?;
            if !said_unread && let Some(unread) = progress::unread(&unread) {
                tell_reader(&format_args!("run {}: {unread}", record.id()))?;
                said_unread = true;
            }
            if let Some(progress) = progress {
                for milestone in progress::milestones(&progress) {
                    if said.insert(milestone.clone()) {
                        tell_reader(&milestone)?;
                    }
                }
            }
        }
        if let Some(ending) = ending {
            return Ok(ending);
        }
        wait(polls.pause())?;
    }
}

pub(crate) fn following(record: &Record, command: &str) {
    say(format_args!(
        "following run {}, recorded in {}: ending `pumpkin {command}` does not stop it",
        record.id(),
        record.dir().display()
    ));
}

/// Prints what `printed` asks for, then says how the run ended, with the commands to run next,
/// and gives the exit status: success if the run finished with a result, [`Exit::Partial`] if it
/// finished without one, and failure if it failed, was interrupted or was lost. `args` are those
/// that run it again.
pub(crate) fn end(
    record: &Record,
    ending: &Ending,
    printed: Printed,
    at: &next::At,
    args: &[String],
) -> Result<Exit, String> {
    let id = record.id();
    let (exit, how) = match ending {
        Ending::Returned { values } => (Exit::Success, returned(values)),
        Ending::NoResult(why) => (Exit::Partial, why.clone()),
        Ending::Stopped(stopped) => (Exit::Failure, stopped.to_string()),
        // It has no `outcome.json` to print.
        Ending::Lost => {
            let how = "is lost: its driver ended without writing outcome.json, so nothing says \
                       how it ended";
            summarize(record);
            say(closing(id, how, record.dir()));
            again(record, args);
            advise("remove what it left", &next::of_run("cancel", at, id));
            advise("remove what every run that has ended left", &next::reap(at));
            return Ok(Exit::Failure);
        }
    };
    printed.print(record.dir())?;
    summarize(record);
    say(closing(id, &how, record.dir()));
    again(record, args);
    Ok(exit)
}

fn summarize(record: &Record) {
    match progress::summary(record) {
        Ok(Some(summary)) => {
            // Its first line is `pumpkin`'s, and the rest are indented beneath it.
            to_stderr(format!("pumpkin: {summary}").as_bytes());
        }
        Ok(None) => {}
        Err(error) => say(format_args!("the run's summary can't be read: {error}")),
    }
}

fn again(record: &Record, args: &[String]) {
    if let Some(build) = record.other_build() {
        say(format_args!(
            "run {} was made by pumpkin {build}, and this is pumpkin {BUILD}: to run it as it \
             ran, run it with the pumpkin that made it",
            record.id()
        ));
    }
    advise("run it again", &next::run_again(args));
}

pub(crate) fn returned(values: &[(Name, Value)]) -> String {
    if values.is_empty() {
        "finished with a result".to_owned()
    } else {
        let shown = progress::shown_values(values);
        format!("finished with a result ({shown})")
    }
}

pub(crate) fn closing(id: &str, how: &str, dir: &Path) -> String {
    format!("run {id} {how}; its record is {}", dir.display())
}
