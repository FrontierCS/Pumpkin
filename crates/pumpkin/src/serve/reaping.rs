//! Stops jobs by their systemd unit and reaps runs of this build using the client's credentials.

use std::fs::File;
use std::io::{self, Write as _};
use std::path::Path;
use std::time::Duration;

use provider::RunTag;
use runtime::Writer;
use stdx::Context as _;
use stdx::process::Environment;
use tempfile::NamedTempFile;

use crate::link::{CancelAnswer, CancelRequest};
use crate::{cli, job};

pub(super) fn cancel(
    home: &Path,
    request: CancelRequest,
    body: &[u8],
) -> Result<CancelAnswer, String> {
    let id = RunTag::try_from(request.id)?;
    let id = id.as_str();
    let jobs = job::jobs()?;
    // A listing can see the durable job directory while Start is still submitting its unit.
    match File::open(&jobs) {
        Ok(jobs) => jobs.lock_shared().context("waiting for the job to start")?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context(jobs.display()),
    }
    if !super::exists(&home.join(job::JOBS).join(id))? {
        return Err(format!("there is no job {id} here"));
    }
    let held = jobs.join(id);
    let record = home.join(job::runs()).join(id);
    if !super::exists(&record)? {
        stop(id, &held, request.force)?;
        // The driver can publish its record between the first check and the stop.
        if !super::exists(&record)? {
            return Ok(CancelAnswer::Unstarted);
        }
    }
    match runtime::writer(&record) {
        Ok(Writer::This) => {}
        writer => {
            // The unit can be stopped even when its record's writer cannot be read.
            stop(id, &held, request.force)?;
            let writer = match writer {
                Ok(Writer::Other(build)) => Some(build),
                _ => None,
            };
            return Ok(CancelAnswer::Stopped { writer });
        }
    }
    let modal = modal(request.modal_bytes.map(|_| body))?;
    let cancel = cli::Cancel {
        run: cli::RunName::Prefix(id.to_owned()),
        runs: None,
        force: request.force,
        modal_config: modal.as_ref().map(|file| file.path().to_owned()),
    };
    let cancelled = crate::cancelled(cancel, crate::Reader::Follower)?;
    Ok(match cancelled.reached {
        Ok(true) => CancelAnswer::Ended,
        Ok(false) => CancelAnswer::EndedUnreached,
        Err(message) => CancelAnswer::EndedUnreaped { message },
    })
}

fn stop(id: &str, held: &Path, force: bool) -> Result<(), String> {
    let unit = format!("pumpkin-job-{id}");
    let call = |args: &[&str]| -> Result<(), String> {
        if !super::exists(held)? {
            return Ok(());
        }
        let mut ctl = stdx::process::command(
            "systemctl",
            Environment::Only(&[
                "HOME",
                "PATH",
                "XDG_RUNTIME_DIR",
                "DBUS_SESSION_BUS_ADDRESS",
            ]),
        );
        ctl.arg("--user").args(args).arg(&unit);
        let stopped = crate::controller::bounded(&mut ctl, "systemctl", Duration::from_mins(10));
        match stopped {
            Err(_) if !super::exists(held)? => Ok(()),
            result => result.map(drop),
        }
    };
    if force {
        call(&["kill", "--signal=KILL"])?;
    }
    call(&["stop"])
}

/// A Modal configuration kept in tmpfs for the duration of this request.
fn modal(body: Option<&[u8]>) -> Result<Option<NamedTempFile>, String> {
    let Some(body) = body else { return Ok(None) };
    let runtime = job::runtime_dir()?;
    let mut file = tempfile::Builder::new()
        .prefix("pumpkin-modal.")
        .tempfile_in(&runtime)
        .context(runtime.display())?;
    file.write_all(body).context(file.path().display())?;
    Ok(Some(file))
}
