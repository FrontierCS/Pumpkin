//! The controller's boot. Each build installed in the controller user's workspace has a
//! `<install>/bin/pumpkin`. The boot runs each one's `controller boot` in turn, which reaps the
//! runs that the build's jobs left. All their output is appended to [`BOOT_LOG`] in the user's
//! home, and `serve` reports the end of that log.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use stdx::Context as _;
use stdx::process::Environment;

use crate::cli::Exit;
use crate::job::BOOT_LOG;
use crate::workspace;

/// Runs each installed build's boot, as the module says. Returns 0, or 1 if a boot failed, which
/// the log then says.
pub(crate) fn boot_all() -> Result<Exit, String> {
    let home = crate::home("run it as the controller's user, whose home holds the builds")?;
    let boots = boots(&home);
    if boots.as_ref().is_ok_and(Vec::is_empty) {
        return Ok(Exit::Success);
    }
    let path = home.join(BOOT_LOG);
    let opened = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(&path);
    let mut log = opened.context(path.display())?;
    let boots = match boots {
        Ok(boots) => boots,
        Err(error) => {
            // The log gets the error too, as `serve` reports the end of the log as the boot's
            // failure.
            let _ = writeln!(log, "pumpkin: {error}");
            return Err(error);
        }
    };
    let mut failed = Vec::new();
    for boot in boots {
        let mut command = stdx::process::command(&boot, Environment::Inherited);
        command.args(["controller", "boot"]);
        let streams = log.try_clone().and_then(|out| Ok((out, log.try_clone()?)));
        let (out, err) = streams.context(path.display())?;
        // Unbounded, as each boot bounds its own reaps by their providers' deadlines.
        let ended = stdx::process::spawn(&mut command, Stdio::null(), out.into(), err.into())
            .and_then(|mut child| child.wait().context(boot.display()));
        let why = match ended {
            Ok(status) if status.success() => continue,
            Ok(status) => match status.code() {
                Some(code) => format!("exit {code}"),
                None => status.to_string(),
            },
            Err(error) => error,
        };
        failed.push(format!("{} ({why})", boot.display()));
    }
    if failed.is_empty() {
        return Ok(Exit::Success);
    }
    let failed = failed.join(" ");
    writeln!(log, "controller boot failed for {failed}").context(path.display())?;
    Ok(Exit::Failure)
}

/// The `pumpkin` of each build installed below `home`, in order.
fn boots(home: &Path) -> Result<Vec<PathBuf>, String> {
    let installs = home.join(workspace::DIR);
    let entries = match fs::read_dir(&installs) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context(installs.display()),
    };
    let mut boots = Vec::new();
    for entry in entries {
        let entry = entry.context(installs.display())?;
        // The workspace's other directories, such as its runs, hold no `bin/pumpkin`.
        let boot = entry.path().join("bin").join(install::File::Pumpkin.name());
        // A link that leads nowhere is a boot that fails, and the log says so.
        match fs::symlink_metadata(&boot) {
            Ok(_) => boots.push(boot),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context(boot.display()),
        }
    }
    boots.sort();
    Ok(boots)
}
