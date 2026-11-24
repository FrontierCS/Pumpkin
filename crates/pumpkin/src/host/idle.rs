//! The controller's idle stop. The controller powers off, and keeps its volume, once it has had no
//! job and no SSH session for [`IDLE`], and has been up that long. Each check that finds a job or a
//! session sets the time of [`BUSY`].

use std::fs::{self, File};
use std::io;
use std::time::{Duration, SystemTime};

use stdx::Context as _;
use stdx::process::Failure;

use super::{command, ran, run};
use crate::cli::Exit;
use crate::controller::USER;
use crate::job::UNIT_PREFIX;

/// How long the controller has been idle before it stops.
const IDLE: Duration = Duration::from_mins(30);

const DIR: &str = "/var/lib/pumpkin";

/// The file whose time is that of the last check that found the controller busy.
const BUSY: &str = "/var/lib/pumpkin/busy";

/// Checks whether the controller is idle, and powers it off if so, as the module says.
pub(crate) fn idle() -> Result<Exit, String> {
    if jobs()? || ssh()? {
        fs::create_dir_all(DIR).context(DIR)?;
        let touched = File::options().create(true).append(true).open(BUSY);
        touched
            .and_then(|busy| busy.set_modified(SystemTime::now()))
            .context(BUSY)?;
        return Ok(Exit::Success);
    }
    let busy_lately = match fs::metadata(BUSY) {
        // A time ahead of the clock counts as a busy check too.
        Ok(busy) => !matches!(busy.modified().context(BUSY)?.elapsed(), Ok(since) if since >= IDLE),
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error).context(BUSY),
    };
    if !busy_lately && crate::serve::up_seconds()? >= IDLE.as_secs() {
        ran("systemctl", &["poweroff"])?;
    }
    Ok(Exit::Success)
}

/// Whether a job's unit is active, among the units of `USER`'s systemd.
fn jobs() -> Result<bool, String> {
    let machine = format!("--machine={USER}@");
    let pattern = format!("{UNIT_PREFIX}*");
    let args = [
        "--user",
        &machine,
        "list-units",
        "--state=active",
        "--no-legend",
        &pattern,
    ];
    let listed = run(command("systemctl", &args)).map_err(|failure| failure.to_string())?;
    Ok(!listed.stdout.trim_ascii().is_empty())
}

/// Whether an SSH session is open, among the sessions that logind lists.
fn ssh() -> Result<bool, String> {
    let listed = run(command("loginctl", &["list-sessions", "--no-legend"]));
    let listed = stdx::process::printed(listed.map_err(|failure| failure.to_string())?)?;
    // Each line starts with the session's ID.
    let ids: Vec<&str> = listed
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    if ids.is_empty() {
        return Ok(false);
    }
    let args = [&["show-session", "--property=Service", "--value"], &ids[..]].concat();
    let shown = match run(command("loginctl", &args)) {
        Ok(shown) => shown,
        // This fails when a listed session has closed since. The others may still be SSH's.
        Err(Failure::Exited { .. }) => return Ok(true),
        Err(failure) => return Err(failure.to_string()),
    };
    let shown = stdx::process::printed(shown)?;
    Ok(shown.lines().any(|service| service == "sshd"))
}
