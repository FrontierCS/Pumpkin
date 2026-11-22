//! The controller's boot unit, `pumpkin-boot.service`. The unit runs each build's reap ([`boot`]).
//! `systemctl` gives the unit's state.

use std::fs::File;
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use stdx::process::Environment;
use stdx::{Context as _, Deadline};

use crate::cli;
use crate::link::Boot;

// The bound of `systemctl show`, which reads a unit's properties from systemd.
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(10);

/// The unit that reaps at the controller's boot.
pub(crate) const BOOT_UNIT: &str = "pumpkin-boot.service";

/// The file in the controller user's home that the boot's output is appended to.
pub(crate) const BOOT_LOG: &str = "boot.log";

/// Reaps what this build's ended or lost runs may have left, using each job's configuration.
/// Returns the reap's exit status, 0 though a provider can't be reached.
pub(crate) fn boot() -> Result<ExitCode, String> {
    // No prune, which each job's start makes: a client may start a job as soon as the
    // controller's `sshd` admits logins, before the boot is done, and the boot, a unit of the
    // system's, can't see that it runs, as systemd names the user's runtime directory, which
    // holds each job's directory, to the user's own units alone. A prune here would remove the
    // images that the job has built, which no IR that the boot reads names yet.
    let reap = cli::Reap {
        runs: None,
        modal_config: None,
    };
    crate::reap(reap, crate::Reaper::Boot)
}

/// The state of the controller's boot unit, and of its last run if it failed.
pub(crate) fn state(home: &Path) -> Boot {
    let shown = Shown::of(&["ActiveState", "ExecMainStatus", "ExecMainExitTimestamp"]);
    match shown.get("ActiveState") {
        None | Some("") => Boot::Unknown,
        Some("activating") => Boot::Running,
        Some("failed") => match shown.get("ExecMainStatus").map(str::parse) {
            Some(Ok(status)) => Boot::Failed {
                status,
                log: boot_log(&home.join(BOOT_LOG)).unwrap_or_else(|error| error.to_string()),
                at: shown
                    .get("ExecMainExitTimestamp")
                    .filter(|at| !at.is_empty())
                    .map(str::to_owned),
            },
            Some(Err(_)) | None => Boot::Unknown,
        },
        Some(_) => Boot::Done,
    }
}

/// Returns the jobs' directory `jobs`, locked as a prune locks it (`super::open_jobs`), once the
/// boot unit has run to its end in this boot, with no job of it queued. Returns `None` until then,
/// and while `systemctl` can't say.
// The boot's reap of an old build may need what the prune would remove.
pub(super) fn after(jobs: &Path) -> Result<Option<File>, String> {
    let locked = super::open_jobs(jobs)?;
    locked.lock().context(super::jobs_named(jobs))?;
    let shown = Shown::of(&["ActiveState", "Job", "ExecMainExitTimestamp"]);
    // systemd keeps a unit's times for the boot alone, so an exit time is of this boot's run.
    let ended = shown
        .get("ExecMainExitTimestamp")
        .is_some_and(|value| !value.is_empty());
    let idle = shown.get("Job") == Some("") && shown.get("ActiveState") != Some("activating");
    Ok((ended && idle).then_some(locked))
}

/// The boot unit's properties, as `systemctl show` gives them. They are empty if `systemctl`
/// doesn't run or fails.
struct Shown(String);

impl Shown {
    fn of(properties: &[&str]) -> Self {
        let mut systemctl = stdx::process::command("systemctl", Environment::Only(&["PATH"]));
        systemctl.arg("show");
        systemctl.args(properties.iter().map(|name| format!("--property={name}")));
        systemctl.arg(BOOT_UNIT);
        let deadline = Deadline::after(SYSTEMCTL_TIMEOUT);
        match stdx::process::output(&mut systemctl, deadline) {
            Ok(Some(output)) => Self(String::from_utf8_lossy(&output.stdout).into_owned()),
            Ok(None) | Err(_) => Self(String::new()),
        }
    }

    fn get(&self, name: &str) -> Option<&str> {
        (self.0.lines()).find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
    }
}

/// The last lines of the boot unit's output, small enough to fit in the health frame even
/// when the JSON encoding escapes every byte.
fn boot_log(path: &Path) -> io::Result<String> {
    const TAIL_BYTES: u64 = 4096;
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => return Err(error),
    };
    let size = file.metadata()?.len();
    file.seek(SeekFrom::Start(size.saturating_sub(TAIL_BYTES)))?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<_> = text.lines().rev().take(5).collect();
    lines.reverse();
    Ok(lines.join("\n"))
}
