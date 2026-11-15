use std::fmt;
use std::fs::File;
use std::io::{self, Read as _};
use std::path::Path;
use std::process::ExitCode;

use runtime::{BUILD, Ending, Record};
use stdx::Context;

use crate::{Reader, advise, escaped, next, print_outcome, progress, say, tell, to_stderr};

/// The most bytes of a final judge's standard error that a run's ending shows ([`end`]): one
/// that holds more is named, and not shown.
// A judge's diagnostic, such as a traceback, fits many times over; more is a log, which the record
// keeps for reading, rather than a message for a terminal.
const JUDGE_STDERR_SHOWN_BYTES: u64 = 1 << 20;

/// Waits until the run of `record`, in `runs`, as commands name it ([`end`]), has ended, saying
/// first, if it is live, that ending `pumpkin command` does not stop it ([`following`]), then its
/// milestones as they come ([`followed`]), and prints its outcome; returns the exit status that
/// `wait`'s help gives.
pub(crate) fn follow(
    record: &Record,
    command: &str,
    runs: Option<&str>,
) -> Result<ExitCode, String> {
    let ending = if let Some(ending) = record.ending()? {
        ending
    } else {
        following(record, command);
        followed(record, Reader::Operator)?
    };
    end(record, &ending, runs, record.args())
}

// A milestone said within a second is timely, and each poll reads only what the log gained, and
// each sandbox's `sandbox.json`.
const FOLLOW_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// Waits until the run of `record`, which was live, has ended, or been lost, and returns how;
/// says each of its milestones meanwhile, as its log records it ([`progress::milestones`]), and,
/// once, the parts of its log that can't be read, if any ([`progress::unread`]), to `reader`.
/// Fails if a follower can't be told.
pub(crate) fn followed(record: &Record, reader: Reader) -> Result<Ending, String> {
    let mut said = std::collections::BTreeSet::new();
    let mut said_unread = false;
    let mut log = record.fold();
    let tell_reader = |message: &dyn fmt::Display| match reader {
        Reader::Operator => {
            say(message);
            Ok(())
        }
        // Its standard error is a connection's end, so a write that fails says that the other end
        // has closed, and nothing else would end this loop before the run does.
        Reader::Follower => tell(message).context(format_args!(
            "telling the client of run {} failed",
            record.id()
        )),
    };
    loop {
        // How it ended first, so that the log read after holds every milestone before the end.
        let ending = record.ending()?;
        if let Some(started_over) = log.take()? {
            tell_reader(&format_args!("run {}: {started_over}", record.id()))?;
        }
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
        if let Some(ending) = ending {
            return Ok(ending);
        }
        std::thread::sleep(FOLLOW_POLL);
    }
}

/// Says that `pumpkin command` follows the run of `record`, and that ending it does not stop
/// the run.
pub(crate) fn following(record: &Record, command: &str) {
    say(format_args!(
        "following run {}, recorded in {}: ending `pumpkin {command}` does not stop it",
        record.id(),
        record.dir().display()
    ));
}

/// Prints the outcome of `record`, a run that has `ended`, then says its summary
/// ([`summarize`]) and how it ended ([`closing`]), after the standard error of a final judge
/// that returned no result ([`show_judge_stderr`]), or why it can't be found; or of a lost run,
/// says its summary, and that it is lost. Ends with the command that runs it again with `args`
/// ([`again`]), and of a lost run, those that remove what it left, which name its runs directory
/// `runs`, absolute, or none if it is the default. Returns the exit status that `run`'s and
/// `wait`'s help give.
pub(crate) fn end(
    record: &Record,
    ending: &Ending,
    runs: Option<&str>,
    args: &[String],
) -> Result<ExitCode, String> {
    let id = record.id();
    let (exit, how, judge_stderr) = match ending {
        Ending::Scored { score, after } => {
            let score = progress::shown_score(score);
            let how = format!("finished with a score of {score}, {after}");
            (ExitCode::SUCCESS, how, None)
        }
        Ending::Unscored(why) => (ExitCode::from(3), why.clone(), None),
        Ending::NoResult { why, judge_stderr } => {
            (ExitCode::from(3), why.clone(), Some(judge_stderr))
        }
        Ending::Failed(how) | Ending::Interrupted(how) => (ExitCode::FAILURE, how.clone(), None),
        // It has no `outcome.json` to print.
        Ending::Lost => {
            let how = "is lost: its driver ended without writing outcome.json, so nothing says \
                       how it ended";
            summarize(record);
            say(closing(id, how, record.dir()));
            again(record, args);
            advise("remove what it left", &next::of_run("cancel", runs, id));
            advise(
                "remove what every run that has ended left",
                &next::reap(runs),
            );
            return Ok(ExitCode::FAILURE);
        }
    };
    print_outcome(record.dir())?;
    summarize(record);
    match judge_stderr {
        Some(Ok(path)) => show_judge_stderr(path),
        Some(Err(error)) => say(format_args!(
            "the final judge's standard error can't be found: {}",
            escaped(error)
        )),
        None => {}
    }
    say(closing(id, &how, record.dir()));
    again(record, args);
    Ok(exit)
}

/// Says the summary of the run of `record`, which has ended ([`progress::summary`]), if it has a
/// log, or why it can't be read.
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

/// Says the command that runs the run of `record` again, with `args`, those of a `pumpkin run`.
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

/// The line that closes a run's ending on the standard error: the run `id`, `how` it ended, as
/// an [`Ending`] says, and where its record is, `dir`.
pub(crate) fn closing(id: &str, how: &str, dir: &Path) -> String {
    format!("run {id} {how}; its record is {}", dir.display())
}

/// Shows on the standard error the final judge's standard error, which its record holds at
/// `path`, under a line that names it ([`indented`]). One of more than
/// [`JUDGE_STDERR_SHOWN_BYTES`], or that can't be read, is named alone; one that is empty, or
/// wasn't collected, is not shown.
fn show_judge_stderr(path: &Path) {
    let named = format!("the final judge's standard error, {},", path.display());
    let mut bytes = Vec::new();
    let read = File::open(path).and_then(|file| {
        let size = file.metadata()?.len();
        if size <= JUDGE_STDERR_SHOWN_BYTES {
            file.take(JUDGE_STDERR_SHOWN_BYTES)
                .read_to_end(&mut bytes)?;
        }
        Ok(size)
    });
    match read {
        Ok(size) if size > JUDGE_STDERR_SHOWN_BYTES => say(format_args!(
            "{named} holds {size} bytes, more than the {JUDGE_STDERR_SHOWN_BYTES} shown here: read \
             it there"
        )),
        Ok(_) if bytes.is_empty() => {}
        Ok(_) => {
            say(format_args!("{named} holds:"));
            to_stderr(indented(&bytes).as_bytes());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => say(format_args!("{named} can't be read: {error}")),
    }
}

/// `bytes`, which a sandbox wrote, as text for a terminal: each line indented, and each control
/// character but a tab escaped.
fn indented(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut shown = String::new();
    for line in text.strip_suffix('\n').unwrap_or(&text).split('\n') {
        shown.push_str("  ");
        shown.push_str(&escaped(line));
        shown.push('\n');
    }
    shown
}
