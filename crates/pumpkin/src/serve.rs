//! `pumpkin controller serve`: the controller's end of a client's link ([`crate::link`]), on its
//! standard input and output, which answers from the runs, the jobs and the machine there.
//!
//! It answers each request until its input ends or brings no request for [`IDLE`]. As it starts,
//! and at each request, it marks its install as used, and the prune of installs reads that mark.
//! So a `serve` that an open page keeps for days keeps its install. It refuses input
//! that breaks the protocol, and exits 1.
//!
//! It acts for the member that `--member` names, as the controller's forced command gives it: each
//! job it starts names that member, its listings name each job's, and it cancels another member's
//! job only when asked to.

mod copy;
mod follow;
mod reaping;

use std::fs::{self, File};
use std::io::{self, BufReader, Write};
use std::os::fd::AsFd;
use std::path::Path;
use std::time::{Duration, SystemTime};

use engine::prune::Disk;
use members::Member;
use provider::{RunTag, UnixTime};
use rustix::event::{self, PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use schema::frame;
use stdx::Context as _;

use crate::cli::Exit;
use crate::job::{JOBS, WAITING};
use crate::link::copy::CopiedRequest;
use crate::link::{
    self, Ask, Health, HealthRequest, Hello, Job, Jobs, JobsRequest, ListRequest, Listed, Owner,
    ViewRequest,
};
use crate::manifest::store::{Missing, PutAnswer};
use crate::progress::{self, Entry, Listing, Row};
use crate::say;
use crate::view::Answering;

/// How long to wait for a request before ending: a page that is open polls every few seconds, so
/// a minute with none is a closed page or a lost connection, and ending closes its SSH connection.
pub(crate) const IDLE: Duration = Duration::from_mins(1);

const UPTIME: &str = "/proc/uptime";

/// Answers a client's link on the standard input and output, as the module says, for `member`.
pub(crate) fn serve(member: &Member) -> Result<Exit, String> {
    let home = crate::home("run `serve` as the controller's user, whose home holds the runs")?;
    marked();
    let stdin = io::stdin().as_fd().try_clone_to_owned();
    let mut input = BufReader::new(File::from(stdin.context("the standard input")?));
    let mut output = io::stdout().lock();
    if !delivered(frame::write(&mut output, &Hello::Serving, &[]))? {
        return Ok(Exit::Success);
    }
    requests(&mut input, &mut output, &home, member)
}

fn requests(
    input: &mut BufReader<File>,
    output: &mut impl Write,
    home: &Path,
    member: &Member,
) -> Result<Exit, String> {
    // Kept for the link's life: it holds what it read of the live runs.
    let mut view = Answering::new(home);
    while arrives(input)? {
        let ask = match request_head(input) {
            Ok(Some(ask)) => ask,
            Ok(None) => break,
            Err(error) => return invalid_request(output, error),
        };
        marked();
        let body = match ask.read_buffered_body(input) {
            Ok(body) => body,
            Err(error) => return invalid_request(output, error),
        };
        let written = match ask {
            Ask::Copy(request) => copy::answer(output, home, request, &body),
            Ask::Copied(request) => link::answer(output, request, |CopiedRequest { id }| {
                crate::job::mark_copied(home, &RunTag::try_from(id)?, member)
            }),
            Ask::Lacks(request) => {
                link::answer(output, request, |request| missing(home, &request, &body))
            }
            Ask::Put(put) => match receive_put(home, &put, input) {
                Ok(Ok(answer)) => frame::write(output, &answer, &[]),
                Ok(Err(message)) => link::refuse(output, message),
                Err(error) => match error.kind() {
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe => break,
                    _ => return invalid_request(output, error),
                },
            },
            Ask::Puts { blobs } => match receive_group(home, &blobs, input) {
                Ok(answers) => output.write_all(&answers).and_then(|()| output.flush()),
                Err(error) => match error.kind() {
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe => break,
                    _ => return invalid_request(output, error),
                },
            },
            Ask::Start(request) => link::answer(output, request, |request| {
                let (start, credentials) = request.decode(&body)?;
                crate::job::start(home, start, credentials, member)
            }),
            Ask::Reap(request) => reaping::reap(output, home, request, &body),
            Ask::Cancel(request) => reaping::cancel(output, home, request, &body, member),
            Ask::Follow {
                id,
                log_from_bytes,
                after_cut,
            } => follow::answer(output, home, id, log_from_bytes, after_cut),
            Ask::Health(request) => link::answer(output, request, |HealthRequest {}| health(home)),
            Ask::Jobs(request) => {
                link::answer(output, request, |JobsRequest {}| jobs(home, member))
            }
            Ask::List(request) => link::answer(output, request, |ListRequest { prefixes }| {
                listed(home, &prefixes)
            }),
            Ask::View(request) => {
                link::answer(output, request, |ViewRequest { target, deflate }| {
                    Ok(view.answer(&target, deflate))
                })
            }
        };
        if !delivered(written)? {
            break;
        }
    }
    Ok(Exit::Success)
}

/// The runs' jobs in `home`, each with its member, for a link that acts for `member`.
fn jobs(home: &Path, member: &Member) -> Result<Jobs, String> {
    let dir = home.join(JOBS);
    let ids = crate::job::ids(&dir)?;
    let jobs = ids.into_iter().map(|id| {
        let owner = Owner::from(crate::job::member(&dir.join(&id)));
        Job { id, owner }
    });
    Ok(Jobs {
        member: member.clone(),
        jobs: jobs.collect(),
    })
}

fn request_head(input: &mut impl io::Read) -> io::Result<Option<Ask>> {
    match frame::read::<Ask>(input)? {
        None => Ok(None),
        Some((ask, body)) if body.is_empty() => Ok(Some(ask)),
        Some((_, body)) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a request has a {}-byte body", body.len()),
        )),
    }
}

fn invalid_request(output: &mut impl Write, error: impl std::fmt::Display) -> Result<Exit, String> {
    let message = format!("{error}: expected a request of this pumpkin");
    delivered(link::refuse(output, message))?;
    Ok(Exit::Failure)
}

fn missing(home: &Path, request: &link::store::Lacks, body: &[u8]) -> Result<Vec<Missing>, String> {
    let manifest = crate::manifest::Manifest::parse(body)?;
    if manifest.name() != request.manifest {
        return Err(
            "the manifest that arrived differs from its digest: send the task again".to_owned(),
        );
    }
    crate::manifest::store::lacks(&home.join(crate::job::STORE), &manifest)
}

fn receive_put(
    home: &Path,
    put: &link::store::Put,
    input: &mut impl io::Read,
) -> io::Result<Result<PutAnswer, String>> {
    let mut parts = link::store::Parts::new(input, put.body_bytes()?);
    let stored = crate::manifest::store::put(
        &home.join(crate::job::STORE),
        &put.digest,
        put.from,
        put.size_bytes,
        &mut parts,
    );
    parts.finish()?;
    Ok(stored)
}

fn receive_group(
    home: &Path,
    puts: &[link::store::Put],
    input: &mut impl io::Read,
) -> io::Result<Vec<u8>> {
    link::store::check_group(puts)?;
    // A refusal can fill the return pipe while the client is still writing the group's bodies.
    let mut answers = Vec::new();
    let mut failed = false;
    for put in puts {
        if failed {
            link::store::Parts::new(input, put.body_bytes()?).finish()?;
        } else {
            let stored = receive_put(home, put, input)?;
            failed = !matches!(stored, Ok(PutAnswer::Stored));
            match stored {
                Ok(answer) => frame::write(&mut answers, &answer, &[])?,
                Err(error) => link::refuse(&mut answers, error)?,
            }
        }
    }
    Ok(answers)
}

// The client may leave at any frame, including while an answer is still streaming.
fn delivered(written: io::Result<()>) -> Result<bool, String> {
    match written {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(error) => Err(error).context("writing to the standard output failed"),
    }
}

/// Whether `input` brings a request, or its end, within [`IDLE`].
pub(crate) fn arrives(input: &BufReader<File>) -> Result<bool, String> {
    if !input.buffer().is_empty() {
        return Ok(true);
    }
    ready(input.get_ref(), PollFlags::IN, IDLE).context("polling the standard input failed")
}

fn ready(fd: &impl AsFd, events: PollFlags, timeout: Duration) -> io::Result<bool> {
    let Ok(timeout) = Timespec::try_from(timeout) else {
        unreachable!("{timeout:?} fits a timespec")
    };
    let mut polled = [PollFd::new(fd, events)];
    loop {
        match event::poll(&mut polled, Some(&timeout)) {
            Ok(count) => return Ok(count != 0),
            Err(Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

/// Marks this `serve`'s install as used now, as [`mark_used`] does. Says what it can't do, and goes
/// on.
fn marked() {
    if let Err(error) = mark_used() {
        say(error);
    }
}

/// Marks this `serve`'s install as used now. The install is the directory above the `bin/` that
/// holds the executable.
fn mark_used() -> Result<(), String> {
    let exe = crate::workspace::exe("expected it to mark its install as used")?;
    let Some(install) = exe.parent().and_then(Path::parent) else {
        return Err(format!("{} is in no install's `bin/`", exe.display()));
    };
    let marked = File::open(install).and_then(|dir| dir.set_modified(SystemTime::now()));
    marked.context(install.display())
}

/// The whole seconds since the machine booted.
pub(crate) fn up_seconds() -> Result<u64, String> {
    let uptime = fs::read_to_string(UPTIME).context(UPTIME)?;
    let seconds = uptime.split(['.', ' ']).next().unwrap_or_default();
    seconds.parse().map_err(|_| {
        format!(
            "{UPTIME} holds `{}`: expected the seconds since the machine booted, first, as \
             Linux writes them",
            uptime.trim().escape_debug()
        )
    })
}

fn health(home: &Path) -> Result<Health, String> {
    let up_seconds = up_seconds()?;
    let Disk {
        size_bytes,
        free_bytes,
    } = crate::job::disk(crate::job::VOLUME)?;
    Ok(Health {
        up_seconds,
        disk_size_bytes: size_bytes,
        disk_free_bytes: free_bytes,
        boot: crate::job::boot_state(home),
    })
}

/// The listing of the runs below `home` whose ids start with one of `prefixes`, as
/// [`crate::listed_in`] makes it, at this machine's time, with the jobs yet to start a run.
fn listed(home: &Path, prefixes: &[String]) -> Result<Listed, String> {
    let now = crate::unix_now();
    let since = progress::listed_since(prefixes, now);
    let runs = home.join(crate::job::runs());
    let (mut listing, mut unlisted) = crate::listed_in(&runs, prefixes, since, now, None)?;
    unstarted(home, prefixes, since, now, &mut listing)?;
    let jobs = home.join(JOBS);
    // A run whose member can't be read is left out and said, as one whose record can't be.
    listing.runs.retain(|id, entry| {
        let Entry::Row(row) = entry else {
            return true;
        };
        match crate::job::run_member(&jobs.join(id)) {
            Ok(member) => {
                row.member = member;
                true
            }
            Err(error) => {
                say(format_args!("run {id}: {error}"));
                unlisted += 1;
                false
            }
        }
    });
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
