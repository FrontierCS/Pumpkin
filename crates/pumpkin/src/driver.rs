//! The driver, the process that drives a run, and how `pumpkin run` starts it and learns that
//! the run has started.
//!
//! `run` starts the driver by executing itself under the name [`ARGV0`], in the process group
//! `run` is in, and the driver first asks the kernel to kill it if `run` ends
//! ([`die_with_run`]). So until the run has started, what ends `run`, a signal from its
//! terminal or one sent to it alone, ends the driver too. The driver
//! reads the configuration and the upstream's key itself: the key never crosses argv, the
//! environment or the pipe.
//!
//! The driver's first argument is `run`'s process ID, its second the run's id, which `run` mints
//! ([`runtime::new_id`]), its third who reads what it says ([`crate::Reader`]), and the rest are
//! `run`'s own: the driver also builds the task's images. Its standard output is a pipe
//! to `run`, which carries at most two lines: the run's id once its record is locked
//! ([`locked`]), then [`STARTED`] once the run has started ([`announce`]). Before the second,
//! the driver withdraws the kernel's signal and leaves `run`'s session, so no signal aimed at
//! `run` reaches a run that `run` says has started.

use std::ffi::{CStr, OsStr, OsString};
use std::fs::OpenOptions;
use std::io::{self, BufRead as _, BufReader, PipeReader};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, ExitStatus, Stdio};

use provider::RunTag;
use runtime::Record;
use rustix::process::{Signal, set_parent_process_death_signal};
use rustix::stdio::{dup2_stderr, dup2_stdout};
use stdx::Context;
use stdx::process::Environment;

/// The name that `run` gives the driver as its `argv[0]`, by which `main` knows it, and that
/// the driver gives its process, as `ps` shows it.
pub(crate) const ARGV0: &CStr = c"pumpkin-driver";

const STARTED: &str = "started";

/// A driver that `run` has started, and not yet heard from.
pub(crate) struct Driver {
    child: Child,
    pipe: BufReader<PipeReader>,
}

pub(crate) enum Handoff {
    Started(Record),
    /// The driver ended, with this status, before the run started, and after it made this
    /// record, if it made one.
    Ended(ExitStatus, Option<Record>),
}

impl Driver {
    /// Starts the driver of the run `id`, which takes this process's ID, `id` and `reader`, then
    /// parses `args` as `pumpkin run`'s.
    pub(crate) fn spawn(
        id: &RunTag,
        reader: crate::Reader,
        args: &[OsString],
    ) -> Result<Self, String> {
        let (pipe, output) = io::pipe().context("making the driver's pipe failed")?;
        // `pumpkin` itself, even if its file is replaced meanwhile.
        let mut command = stdx::process::command("/proc/self/exe", Environment::Inherited);
        command
            .arg0(OsStr::from_bytes(ARGV0.to_bytes()))
            .arg(std::process::id().to_string())
            .arg(id.as_str())
            .arg(reader.word())
            .args(args);
        let child =
            stdx::process::spawn(&mut command, Stdio::null(), output.into(), Stdio::inherit());
        let child = child.context("starting the driver failed")?;
        // The command holds the pipe's write end, which must close here for its read to end
        // once the driver has closed its own.
        drop(command);
        Ok(Self {
            child,
            pipe: BufReader::new(pipe),
        })
    }

    /// Waits until the driver has started the run, whose record it makes in `runs`, or ended.
    pub(crate) fn started(mut self, runs: &Path) -> Result<Handoff, String> {
        let id = self.line()?;
        if let Some(id) = &id
            && self.line()?.as_deref() == Some(STARTED)
        {
            return Ok(Handoff::Started(Record::open(&runs.join(id))?));
        }
        let status = self.child.wait().context("waiting for the driver failed")?;
        let record = id.map(|id| Record::open(&runs.join(id))).transpose()?;
        Ok(Handoff::Ended(status, record))
    }

    /// The next line from the driver, if it sent one before it closed the pipe.
    fn line(&mut self) -> Result<Option<String>, String> {
        let mut line = String::new();
        (self.pipe.read_line(&mut line)).context("reading from the driver failed")?;
        // The driver writes each line whole in one `write` shorter than `PIPE_BUF`, so a line
        // comes whole or not at all.
        Ok(line.strip_suffix('\n').map(str::to_owned))
    }
}

/// Asks the kernel to kill the driver, by SIGKILL, when `run`, whose process ID is `run_pid`,
/// ends, until [`announce`] withdraws that. `run` spawns the driver from its main thread, which
/// lasts as long as `run` does. Fails if `run` has ended already.
pub(crate) fn die_with_run(run_pid: u32) -> Result<(), String> {
    let asked = set_parent_process_death_signal(Some(Signal::KILL));
    asked.context("asking the kernel to end the driver with `run` failed")?;
    // If `run` ended before the kernel was asked, the signal is not coming.
    if std::os::unix::process::parent_id() != run_pid {
        return Err("`run` ended before its driver asked to end with it".to_owned());
    }
    Ok(())
}

/// Tells `run` the id of the run whose record the driver has locked.
pub(crate) fn locked(id: &str) -> Result<(), String> {
    send(id).context("telling `run` the run's id failed")
}

/// Makes the driver outlive `run`, now that the run whose record is `record` has started, tells
/// `run` so, and points the driver's standard output and error at the record's `driver.log`.
/// Fails before `run` is told if `driver.log` can't be opened, the kernel's signal withdrawn or
/// `run`'s session left, and after it if the output can't be pointed at `driver.log`.
pub(crate) fn announce(record: &Path) -> Result<(), String> {
    let path = record.join("driver.log");
    let log = OpenOptions::new().append(true).open(&path);
    let log = log.context(path.display())?;
    let outliving = set_parent_process_death_signal(None);
    outliving.context("asking the kernel to let the driver outlive `run` failed")?;
    // From here to the new session, a signal to `run` alone leaves the run going, unannounced,
    // and one to `run`'s process group ends `run` and interrupts the run before `run` hears of
    // the start: neither has `run` say that a run started which a signal then ends.
    rustix::process::setsid().context("leaving `run`'s session failed")?;
    // A `run` that can't be told has ended, so the run goes on unannounced, as its id was said
    // when the record was locked.
    let _ = send(STARTED);
    // Pointing the standard output elsewhere closes the pipe, so `run` hears no more.
    let redirected = dup2_stdout(&log).and_then(|()| dup2_stderr(&log));
    redirected.context(path.display())
}

/// Sends `line` and its newline to `run`, in one `write`.
fn send(line: &str) -> io::Result<()> {
    crate::print(format!("{line}\n").as_bytes())
}
