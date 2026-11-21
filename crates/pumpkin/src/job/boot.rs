//! The controller's boot: the reap that `pumpkin-boot.service` runs of each build, and the state
//! of that unit, as `systemctl` gives it.

use std::fs::File;
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use stdx::Deadline;
use stdx::process::Environment;

use crate::cli;
use crate::link::Boot;

// The bound of `systemctl show`, which reads a unit's properties from systemd.
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(10);

const BOOT_UNIT: &str = "pumpkin-boot.service";

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
    let mut systemctl = stdx::process::command("systemctl", Environment::Only(&["PATH"]));
    systemctl.args([
        "show",
        "--property=ActiveState",
        "--property=ExecMainStatus",
    ]);
    systemctl.args(["--property=ExecMainExitTimestamp", BOOT_UNIT]);
    // A `systemctl` that doesn't run, or fails, says no state.
    let printed = match stdx::process::output(&mut systemctl, Deadline::after(SYSTEMCTL_TIMEOUT)) {
        Ok(Some(output)) => String::from_utf8_lossy(&output.stdout).into_owned(),
        Ok(None) | Err(_) => String::new(),
    };
    let property =
        |name: &str| (printed.lines()).find_map(|line| line.strip_prefix(name)?.strip_prefix('='));
    match property("ActiveState") {
        None | Some("") => Boot::Unknown,
        Some("activating") => Boot::Running,
        Some("failed") => match property("ExecMainStatus").map(str::parse) {
            Some(Ok(status)) => Boot::Failed {
                status,
                log: boot_log(&home.join("boot.log")).unwrap_or_else(|error| error.to_string()),
                at: property("ExecMainExitTimestamp")
                    .filter(|at| !at.is_empty())
                    .map(str::to_owned),
            },
            Some(Err(_)) | None => Boot::Unknown,
        },
        Some(_) => Boot::Done,
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
