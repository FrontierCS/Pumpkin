//! `pumpkin controller serve`: the controller's end of a client's link ([`crate::link`]), on its
//! standard input and output, which answers from the runs, the jobs and the machine there.
//!
//! As it starts, it marks its install as used, and removes the other installs, and what an install
//! left half made, that no `serve` used for a day, as Zed's server removes its older binaries as
//! it starts
//! (<https://github.com/zed-industries/zed/blob/e231594b6b8818ea8d668ee2eb73e1fb09300654/crates/remote_server/src/server.rs#L1429-L1449>).
//! Then it answers each request until its input ends, brings no request for [`IDLE`], or breaks
//! the protocol, which it refuses and exits 1. It marks its install as used again as each request
//! arrives, so that a `serve` kept for days by an open page keeps its install.

use std::ffi::OsStr;
use std::fs::{self, DirEntry, File};
use std::io::{self, BufReader, Write};
use std::os::fd::AsFd as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use engine::prune::Disk;
use provider::UnixTime;
use rustix::event::{self, PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use schema::frame;
use stdx::process::Environment;
use stdx::{Context as _, Deadline};

use crate::job::{JOBS, WAITING};
use crate::link::{
    self, Ask, Boot, Health, HealthRequest, Hello, JobsRequest, ListRequest, Listed, ViewRequest,
};
use crate::progress::{self, Entry, Listing, Row};
use crate::say;
use crate::view::Answering;

/// How long to wait for a request before ending: a page that is open polls every few seconds, so
/// a minute with none is a closed page or a lost connection, and ending lets the `ssh` session,
/// and the master it goes over, end too.
pub(crate) const IDLE: Duration = Duration::from_mins(1);

// How long an install lasts that no `serve` uses: a day, as a store's unchecked pieces last, so
// that a build used every day keeps its install, and one left behind is gone by the next.
const INSTALL_KEPT: Duration = Duration::from_hours(24);

// The bound of `systemctl show`, which reads a unit's properties from systemd.
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(10);

const BOOT_UNIT: &str = "pumpkin-boot.service";

const UPTIME: &str = "/proc/uptime";

/// Answers a client's link on the standard input and output, as the module says.
pub(crate) fn serve() -> Result<ExitCode, String> {
    let home = crate::home("run `serve` as the controller's user, whose home holds the runs")?;
    let install = own_install();
    match &install {
        Ok(install) => prune(&home.join(link::installs()), install),
        Err(error) => say(format_args!(
            "{error}, so no install is removed, or marked as used"
        )),
    }
    let stdin = io::stdin().as_fd().try_clone_to_owned();
    let mut input = BufReader::new(File::from(stdin.context("the standard input")?));
    let mut output = io::stdout().lock();
    // Kept for the link's life: it holds what it read of the live runs.
    let mut view = Answering::new(home.join(crate::job::runs()));
    let written = "writing to the standard output failed";
    frame::write(&mut output, &Hello::Serving, &[]).context(written)?;
    while arrives(&input)? {
        let broken = match frame::read::<Ask>(&mut input) {
            Ok(Some((ask, body))) if body.is_empty() => {
                if let Ok(install) = &install {
                    mark_install_used(install);
                }
                answer(&mut output, &home, ask, &mut view).context(written)?;
                continue;
            }
            Ok(Some((_, body))) => format!("a request has a {}-byte body", body.len()),
            Ok(None) => break,
            Err(error) => error.to_string(),
        };
        let message = format!(
            "{broken}: expected a request, with no body, as a client of this pumpkin sends"
        );
        link::refuse(&mut output, message).context(written)?;
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

/// Whether `input` brings a request, or its end, within [`IDLE`].
pub(crate) fn arrives(input: &BufReader<File>) -> Result<bool, String> {
    if !input.buffer().is_empty() {
        return Ok(true);
    }
    let Ok(timeout) = Timespec::try_from(IDLE) else {
        unreachable!("{IDLE:?} fits a `timespec`")
    };
    loop {
        let mut polled = [PollFd::new(input.get_ref(), PollFlags::IN)];
        match event::poll(&mut polled, Some(&timeout)) {
            Ok(0) => return Ok(false),
            Ok(_) => return Ok(true),
            Err(Errno::INTR) => {}
            Err(error) => return Err(format!("polling the standard input failed: {error}")),
        }
    }
}

/// The directory of this `serve`'s install, which holds its executable and the rest of its build.
fn own_install() -> Result<PathBuf, String> {
    let exe = crate::workspace::exe("expected it to tell its install from the others")?;
    match exe.parent() {
        Some(install) => Ok(install.to_owned()),
        None => Err(format!(
            "{} is in no directory, as an install is",
            exe.display()
        )),
    }
}

/// Marks `install`, this `serve`'s, as used, and removes each other entry in `installs` that
/// nothing changed for `INSTALL_KEPT`, saying, but going on past, what it can't do.
fn prune(installs: &Path, install: &Path) {
    let own = install.file_name();
    let entries = match fs::read_dir(installs) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => return say(format_args!("{}: {error}", installs.display())),
    };
    let now = SystemTime::now();
    for entry in entries {
        let pruned = (entry.context(installs.display())).and_then(|entry| pruned(&entry, own, now));
        if let Err(error) = pruned {
            say(format_args!(
                "{error}: expected to remove each install of `serve` there that none used for a \
                 day, which the next `serve` tries again"
            ));
        }
    }
}

/// Marks `install`, this `serve`'s, as used now, saying, but going on past, what it can't do.
fn mark_install_used(install: &Path) {
    if let Err(error) = mark_used(install, SystemTime::now()) {
        say(error);
    }
}

fn mark_used(install: &Path, now: SystemTime) -> Result<(), String> {
    let touched = File::open(install).and_then(|dir| dir.set_modified(now));
    touched.context(install.display())
}

fn pruned(entry: &DirEntry, own: Option<&OsStr>, now: SystemTime) -> Result<(), String> {
    let path = entry.path();
    if Some(entry.file_name().as_os_str()) == own {
        return mark_used(&path, now);
    }
    let modified = entry.metadata().and_then(|metadata| metadata.modified());
    let modified = modified.context(path.display())?;
    if now.duration_since(modified).unwrap_or_default() <= INSTALL_KEPT {
        return Ok(());
    }
    let removed = if entry.file_type().context(path.display())?.is_dir() {
        fs::remove_dir_all(&path)
    } else {
        fs::remove_file(&path)
    };
    match removed {
        // Another `serve`'s prune removed it first.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        removed => removed.context(path.display()),
    }
}

/// Writes the answer to `ask` on `output`, with `view` the view of the runs below `home`: a
/// refusal if what it asks for fails.
fn answer(output: &mut impl Write, home: &Path, ask: Ask, view: &mut Answering) -> io::Result<()> {
    match ask {
        Ask::Health(request) => link::answer(output, request, |HealthRequest {}| health()),
        Ask::Jobs(request) => link::answer(output, request, |JobsRequest {}| {
            crate::job::ids(&home.join(JOBS))
        }),
        Ask::List(request) => link::answer(output, request, |ListRequest { prefixes }| {
            listed(home, &prefixes)
        }),
        Ask::View(request) => link::answer(output, request, |ViewRequest { target, deflate }| {
            Ok(view.answer(&target, deflate))
        }),
    }
}

fn health() -> Result<Health, String> {
    let uptime = fs::read_to_string(UPTIME).context(UPTIME)?;
    let seconds = uptime.split(['.', ' ']).next().unwrap_or_default();
    let up_seconds = seconds.parse().map_err(|_| {
        format!(
            "{UPTIME} holds `{}`: expected the seconds since the machine booted, first, as \
             Linux writes them",
            uptime.trim().escape_debug()
        )
    })?;
    let Disk {
        size_bytes,
        free_bytes,
    } = crate::job::disk(crate::job::VOLUME)?;
    Ok(Health {
        up_seconds,
        disk_size_bytes: size_bytes,
        disk_free_bytes: free_bytes,
        boot: boot(),
    })
}

fn boot() -> Boot {
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
                at: property("ExecMainExitTimestamp")
                    .filter(|at| !at.is_empty())
                    .map(str::to_owned),
            },
            Some(Err(_)) | None => Boot::Unknown,
        },
        Some(_) => Boot::Done,
    }
}

/// The listing of the runs below `home` whose ids start with one of `prefixes`, as
/// [`crate::listed_in`] makes it, at this machine's time, with the jobs yet to start a run.
fn listed(home: &Path, prefixes: &[String]) -> Result<Listed, String> {
    let now = crate::unix_now();
    let since = progress::listed_since(prefixes, now);
    let runs = home.join(crate::job::runs());
    let (mut listing, unlisted) = crate::listed_in(&runs, prefixes, since, now, None)?;
    unstarted(home, prefixes, since, now, &mut listing)?;
    Ok(Listed {
        listing,
        unlisted_count: unlisted as u64,
    })
}

/// Adds to `listing` each job in `home`'s `JOBS` whose id starts with one of `prefixes`, or each
/// with none, that `listing` doesn't hold as a run or as older: `waiting` for a build slot,
/// `building`, or `unstarted` once its unit has ended, but only named as older if its log is
/// from before `since`.
fn unstarted(
    home: &Path,
    prefixes: &[String],
    since: Option<UnixTime>,
    now: UnixTime,
    listing: &mut Listing,
) -> Result<(), String> {
    let jobs = home.join(JOBS);
    let entries = match fs::read_dir(&jobs) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context(jobs.display()),
    };
    let held = crate::job::jobs()?;
    for entry in entries {
        let name = entry.context(jobs.display())?.file_name();
        // As `ls` lists them, which leaves out a name that starts with a dot.
        let Some(id) = name.to_str().filter(|id| !id.starts_with('.')) else {
            continue;
        };
        // A run whose directory is made, or whose record is written, after `listed_in` read the
        // runs is not in `listing` yet, and its job is its row, so that no job drops out.
        if !crate::picked(prefixes, id) || listing.runs.contains_key(id) {
            continue;
        }
        let asked = runtime::minted(id);
        let tmpfs = held.join(id);
        let listed = if exists(&tmpfs.join(WAITING))? {
            progress::Listed::Waiting { asked }
        } else if is_dir(&tmpfs)? {
            progress::Listed::Building { asked }
        } else if logged_since(&jobs.join(id).join("log"), since)? {
            progress::Listed::Unstarted
        } else {
            listing.runs.insert(id.to_owned(), Entry::Older);
            continue;
        };
        let row = Row::of(id, &listed, now, None);
        listing.runs.insert(id.to_owned(), Entry::Row(row));
    }
    Ok(())
}

fn exists(path: &Path) -> Result<bool, String> {
    path.try_exists().context(path.display())
}

fn is_dir(path: &Path) -> Result<bool, String> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context(path.display()),
    }
}

/// Whether `log`, a job's, was written since `since`, or is missing.
fn logged_since(log: &Path, since: Option<UnixTime>) -> Result<bool, String> {
    let Some(since) = since else {
        return Ok(true);
    };
    let modified = match fs::metadata(log).and_then(|metadata| metadata.modified()) {
        Ok(modified) => modified,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error).context(log.display()),
    };
    Ok(UnixTime::of(modified).unwrap_or(crate::EPOCH) >= since)
}
