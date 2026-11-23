//! The controller's boot unit, `pumpkin-boot.service`. The unit runs each build's reap ([`boot`]).
//! `systemctl` gives the unit's state.

use std::fs::File;
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::path::Path;
use std::time::Duration;

use stdx::Context as _;
use stdx::process::{Bound, Echo, Environment, Run};

use crate::cli::{self, Exit};
use crate::link::Boot;

// The bound of `systemctl show`, which reads a unit's properties from systemd.
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(10);

/// The unit that reaps at the controller's boot.
pub(crate) const BOOT_UNIT: &str = "pumpkin-boot.service";

/// The file in the controller user's home that the boot's output is appended to.
pub(crate) const BOOT_LOG: &str = "boot.log";

/// Reaps what this build's ended or lost runs may have left, using each job's configuration.
/// Returns the reap's exit status, 0 though a provider can't be reached.
pub(crate) fn boot() -> Result<Exit, String> {
    // No prune, which each job's start makes: a client may start a job as soon as the
    // controller's `sshd` admits logins, before the boot is done, and the boot, a unit of the
    // system's, can't see that it runs, as systemd names the user's runtime directory, which
    // holds each job's directory, to the user's own units alone. A prune here would remove the
    // images that the job has built, which no IR that the boot reads names yet.
    let reap = cli::Reap {
        runs: None,
        controller: None,
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

/// Locks the jobs' directory `jobs` as a prune does (`super::open_jobs`), and returns the lock once
/// the boot unit has ended (`retention::boot::Unit::ended`). Returns `None` until then, and while
/// `systemctl` can't say.
// The boot's reap of an old build may need what the prune would remove.
pub(super) fn after(jobs: &Path) -> Result<Option<File>, String> {
    let locked = super::open_jobs(jobs)?;
    locked.lock().context(super::jobs_named(jobs))?;
    let shown = Shown::of(&["ActiveState", "Job", "ExecMainExitTimestamp"]);
    let unit = retention::boot::Unit {
        active_state: shown.get("ActiveState"),
        job: shown.get("Job"),
        exit_timestamp: shown.get("ExecMainExitTimestamp"),
    };
    Ok(unit.ended().then_some(locked))
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
        let how = Run::new(Bound::Within(SYSTEMCTL_TIMEOUT), Echo::Silent);
        match stdx::process::run(&mut systemctl, how) {
            Ok(output) => Self(String::from_utf8_lossy(&output.stdout).into_owned()),
            Err(_) => Self(String::new()),
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
