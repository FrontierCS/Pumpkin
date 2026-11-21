//! Stops jobs by their systemd unit and reaps runs of this build.

use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::time::Duration;

use provider::RunTag;
use runtime::Writer;
use stdx::Context as _;
use stdx::process::Environment;

use crate::link::{CancelAnswer, CancelRequest};
use crate::member::Member;
use crate::{cli, job, next};

/// Stops job `request.id`, refusing, with nothing done, one of another member's than `member`
/// unless `request.any_member`.
pub(super) fn cancel(
    home: &Path,
    request: CancelRequest,
    member: Option<&Member>,
) -> Result<CancelAnswer, String> {
    let tag = RunTag::try_from(request.id)?;
    let id = tag.as_str();
    let jobs = job::jobs()?;
    // A listing can see the durable job directory while Start is still submitting its unit.
    match File::open(&jobs) {
        Ok(jobs) => jobs.lock_shared().context("waiting for the job to start")?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context(jobs.display()),
    }
    let dir = home.join(job::JOBS).join(id);
    if !super::exists(&dir)? {
        return Err(format!("there is no job {id} here"));
    }
    if let Some(member) = member
        && !request.any_member
    {
        let any = next::again("cancel", &["--any-member".to_owned()], &[id.to_owned()]);
        // A job whose member can't be read may be another's, so it is refused as theirs is.
        match job::member(&dir) {
            Ok(None) => {}
            Ok(Some(owner)) if owner == *member => {}
            Ok(Some(owner)) => return Err(format!("run {id} is {owner}'s: to stop it: {any}")),
            Err(error) => {
                return Err(format!(
                    "run {id} names a member that can't be read: {error}: to stop it: {any}"
                ));
            }
        }
    }
    let held = jobs.join(id);
    let record = home.join(job::runs()).join(id);
    if !super::exists(&record)? {
        stop(&tag, &held, request.force)?;
        // The driver can publish its record between the first check and the stop.
        if !super::exists(&record)? {
            return Ok(CancelAnswer::Unstarted);
        }
    }
    match runtime::writer(&record) {
        Ok(Writer::This) => {}
        writer => {
            // The unit can be stopped even when its record's writer cannot be read.
            stop(&tag, &held, request.force)?;
            let writer = match writer {
                Ok(Writer::Other(build)) => Some(build),
                _ => None,
            };
            return Ok(CancelAnswer::Stopped { writer });
        }
    }
    let cancel = cli::Cancel {
        run: cli::RunName::Prefix(id.to_owned()),
        runs: None,
        force: request.force,
    };
    let cancelled = crate::cancelled(cancel, crate::Reader::Follower)?;
    Ok(match cancelled.reached {
        Ok(true) => CancelAnswer::Ended,
        Ok(false) => CancelAnswer::EndedUnreached,
        Err(message) => CancelAnswer::EndedUnreaped { message },
    })
}

fn stop(id: &RunTag, held: &Path, force: bool) -> Result<(), String> {
    let unit = job::unit_name(id);
    let calls: &[&[&str]] = if force {
        &[&["kill", "--signal=KILL"], &["stop"]]
    } else {
        &[&["stop"]]
    };
    for &args in calls {
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
        if let Err(error) =
            crate::controller::bounded(&mut ctl, "systemctl", Duration::from_mins(10))
        {
            return if super::exists(held)? {
                Err(error)
            } else {
                Ok(())
            };
        }
    }
    // A concurrent SIGKILL to the unit can interrupt ExecStopPost while it removes the keys.
    match fs::remove_dir_all(held) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        removed => removed.context(held.display()),
    }
}

pub(super) fn reap(home: &Path) -> Result<crate::reaping::ReapedRuns, String> {
    crate::reaping::sweep(&home.join(job::runs()), None, crate::Reaper::Controller)
}
