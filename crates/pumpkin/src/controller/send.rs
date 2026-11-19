//! Sending a run's or a build's job to the controller: the task's files, which go by content to
//! the controller's store, and the command that starts the job from them.
//!
//! The controller says which blobs of the task's manifest its store lacks, and only those are
//! sent, in streams over connections of their own. A connection that ends, or a stream that
//! stalls, costs only the pieces not yet stored: the send asks again what the controller lacks.
//!
//! The start request sends this machine's settings and keys on the link's standard input. The
//! controller writes them to the job's directory in tmpfs and starts its systemd unit, which
//! outlives the connection.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};
use std::{fs, io, panic, thread};

use provider::RunTag;
use rustix::event::{self, PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use schema::ir::Sha256Digest;
use schema::workflow::Workflow;
use stdx::Context as _;
use stdx::process::Environment;

use super::linking::Build;
use super::{AGAIN, Name, Reached, SHORT_TIMEOUT, SSH_FAILED, STALL_TIMEOUT, Session, bounded};
use crate::link::{JobKind, JobStart, StartAnswer, StartRequest};
use crate::{config, manifest, say};

// How often a send says how much it has sent: often enough that a person sees a send move, and
// seldom enough that a log of a send of minutes keeps a few lines of it. BuildKit's plain progress
// likewise says a step's count again only once it has moved 5%, or 5 s have passed:
// https://github.com/moby/buildkit/blob/1d510a3097d9fbd85a42d162d42c3a79eda1f795/util/progress/progressui/printer.go#L93-L101
const PROGRESS: Duration = Duration::from_secs(15);

/// The name of the SSM parameter that holds the settings of the jobs of controller `name`.
fn parameter(name: &Name) -> String {
    format!("/pumpkin-{name}/settings")
}

/// A task's git repository, as this machine sends it to the controller.
pub(super) struct Tree {
    /// The files the task reaches, by content.
    hashed: manifest::Hashed,
    /// The entry's path below the repository's top.
    entry: String,
    /// The repository's name, the top's own as `plain` gives it, which its trees on the
    /// controller are below.
    repo: String,
    /// The task's workflow, as checking it gave it, which isn't sent.
    pub(super) workflow: Workflow,
}

/// Hashes the tracked and unignored files that the task reaches below its repository.
pub(super) fn tree(entry: &crate::star::Entry, args: &lang::Args) -> Result<Tree, String> {
    let path = fs::canonicalize(entry.path()).context(entry.path())?;
    let dir = path.parent().unwrap_or(Path::new("/"));
    let git = |dir: &Path, args: &[&str]| {
        let mut git = stdx::process::command("git", Environment::Only(&["HOME", "PATH"]));
        git.arg("-C").arg(dir).args(args);
        bounded(&mut git, &format!("git {}", args.join(" ")), SHORT_TIMEOUT)
    };
    let top = git(dir, &["rev-parse", "--show-toplevel"]).map_err(|error| {
        format!("{entry}: {error}: a run on the controller sends the entry's git repository: put it in one")
    })?;
    let top = PathBuf::from(String::from_utf8_lossy(&top).trim_end_matches('\n'));
    let top = fs::canonicalize(&top).context(top.display())?;
    let files = git(
        &top,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    let reach = match crate::star::reached(entry, args)? {
        Ok(reach) => reach,
        Err(problems) => return Err(crate::task_problems(entry, &problems)),
    };
    let files = needed(entry, &reach.root, &reach.paths, &top, &files)?;
    let hashed = manifest::hash(&top, &files)?;
    let below = path.strip_prefix(&top).ok().and_then(Path::to_str);
    let Some(below) = below else {
        unreachable!(
            "{} is below {}, as git found it from the entry's directory",
            path.display(),
            top.display()
        )
    };
    let name = top
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let repo = plain(&name);
    Ok(Tree {
        hashed,
        entry: below.to_owned(),
        repo,
        workflow: reach.workflow,
    })
}

/// `name` with each character that isn't ASCII alphanumeric or one of `._-` made `_`, to be a
/// directory's name.
fn plain(name: &str) -> String {
    let kept = |c: char| c.is_ascii_alphanumeric() || "._-".contains(c);
    name.chars()
        .map(|c| if kept(c) { c } else { '_' })
        .collect()
}

/// Of `listed`, the files that git lists in `top`, each ended by a NUL, those that checking the
/// task whose entry is `entry` reaches, `reached` below its `root`, as files or as directories
/// that hold them.
fn needed(
    entry: &crate::star::Entry,
    root: &Path,
    reached: &BTreeSet<String>,
    top: &Path,
    listed: &[u8],
) -> Result<Vec<u8>, String> {
    // At or below the top, unless the marker is above the repository that holds the entry.
    let prefix = root.strip_prefix(top).map_err(|_| {
        format!(
            "{entry}: the task's root, {}, is above its git repository, {}, which is all that a \
             run on the controller is sent: put the root's `{}` in the repository",
            root.display(),
            top.display(),
            crate::star::MARKER
        )
    })?;
    let prefix = prefix.to_string_lossy();
    let prefix = prefix.as_ref();
    let under = |path: &str| match prefix {
        "" => path.to_owned(),
        prefix if path.is_empty() => prefix.to_owned(),
        prefix => format!("{prefix}/{path}"),
    };
    let kept: Vec<String> = reached.iter().map(|path| under(path)).collect();
    let wanted = |file: &str| {
        kept.iter().any(|path| {
            path.is_empty()
                || file == path
                || file
                    .strip_prefix(path.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    };
    let mut files = Vec::new();
    for file in listed
        .split(|byte| *byte == 0)
        .filter(|file| !file.is_empty())
    {
        if wanted(&String::from_utf8_lossy(file)) {
            files.extend_from_slice(file);
            files.push(0);
        }
    }
    Ok(files)
}

/// Sends the task's `tree` and the client's configuration, keys and Modal configuration to the
/// controller, and starts the job `id` of `kind` there, which runs the task with `args`.
/// The keys go on the connection's standard input, to the job's directory in tmpfs, and are not
/// stored.
pub(super) fn send(
    reached: &Reached,
    client: &config::Client,
    id: &RunTag,
    kind: JobKind,
    tree: &Tree,
    args: &lang::Args,
) -> Result<Build, String> {
    send_tree(reached, tree)?;
    let build = Build::of_this("expected to send it to the controller to run the job")?;
    let (sent, keys) = client.sent();
    let mut body = Vec::new();
    let mut key_bytes = Vec::new();
    for key in keys {
        let read = fs::read(&key).map_err(|error| {
            format!(
                "the key file {} can't be read ({error}): put the key there",
                key.display()
            )
        })?;
        key_bytes.push(read.len());
        body.extend(read);
    }
    let modal = client.modal_file()?;
    let modal_bytes = modal.as_ref().map(Vec::len);
    if let Some(modal) = modal {
        body.extend(modal);
    }
    let start = JobStart {
        id: id.as_str().to_owned(),
        kind,
        repo: tree.repo.clone(),
        tree: tree.hashed.manifest.name(),
        words: crate::arg_flags(args)
            .chain(["--".to_owned(), tree.entry.clone()])
            .collect(),
        region: client.controller.region.clone(),
        parameter: parameter(&client.controller.name),
        sent,
        key_bytes,
        modal_bytes,
    };
    let request = StartRequest::encode(&start, &mut body)?;
    match reached.asked(&build, "starting the job", |linked| {
        linked.ask_with_body(reached, request, &body)
    })?? {
        StartAnswer::Started | StartAnswer::AlreadyStarted => Ok(build),
        StartAnswer::Unconfirmed { message } => Err(message),
        StartAnswer::Unadmitted { changed, gone } => {
            Err(manifest::unadmitted(&tree.hashed, &changed, &gone))
        }
    }
}

/// Sends the blobs of the tree that the controller lacks, saying what that is, until it lacks none.
/// Sends of one workspace take turns, so that none sends what another is sending.
fn send_tree(reached: &Reached, tree: &Tree) -> Result<(), String> {
    let manifest::Hashed { manifest, sources } = &tree.hashed;
    if manifest::pieces(&lacked(reached, manifest)?).is_empty() {
        say(holds(&reached.name, manifest, &[]));
        return Ok(());
    }
    let _upload = reached.shared.upload(&reached.name)?;
    let mut attempt = 0;
    let mut lacking = usize::MAX;
    loop {
        let pieces = manifest::pieces(&lacked(reached, manifest)?);
        // A connection that delivered a piece moved the send on, as `pause` counts attempts.
        if pieces.len() < lacking {
            attempt = 0;
        }
        lacking = pieces.len();
        let streams = manifest::deal(pieces);
        say(holds(&reached.name, manifest, &streams));
        if streams.is_empty() {
            return Ok(());
        }
        // Each stream's end is read once all have ended, so that one whose connection ended loses
        // none of what the others sent; any other failure is the send's.
        let mut cut = None;
        for streamed in sent(reached, &streams, sources)? {
            if streamed.cut {
                cut = cut.or(Some(streamed.master));
            } else {
                streamed.ended(reached)?;
            }
        }
        let Some(master) = cut else {
            return Ok(());
        };
        if !reached.pause(attempt, master, "asking again what it lacks") {
            return Err(format!(
                "a connection to controller `{}` ended at {} attempts of the send in a row, the \
                 last as its log says: {}: run it again",
                reached.name,
                AGAIN.len() + 1,
                reached.shared.log(master).display()
            ));
        }
        attempt += 1;
    }
}

/// How one stream of a send ended.
struct Streamed {
    master: usize,
    status: ExitStatus,
    /// Whether the stream's connection ended under it, not the command on the controller.
    cut: bool,
    /// What writing the stream came to: `Err` if a write failed, and `Ok(Err)` if a source changed
    /// since it was hashed.
    written: io::Result<Result<(), String>>,
}

impl Streamed {
    /// Whether the stream, whose connection didn't end, sent all its pieces, else why not.
    fn ended(self, reached: &Reached) -> Result<(), String> {
        let Self {
            master,
            status,
            cut: _,
            written,
        } = self;
        match written {
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                let stalled = format!(
                    "sending the task's files to controller `{}` stalled: {error}, so it was \
                     stopped: run it again",
                    reached.name
                );
                Err(reached.lost(master, stalled))
            }
            Ok(Err(changed)) => Err(changed),
            _ if !status.success() => {
                let failed = format!(
                    "sending the task's files to controller `{}` failed ({status}), as said above",
                    reached.name
                );
                Err(reached.lost(master, failed))
            }
            Ok(Ok(())) => Ok(()),
            Err(error) => Err(format!("writing to `ssh` failed: {error}")),
        }
    }
}

/// Sends each of `streams` at once, each a `manifest::receive` of its own over a session of a
/// connection that no other of them holds, as long as one can be had, and each as its connection
/// opens, while it says how far the send is. Returns how each stream ended, once all have.
// Many connections at once, each with a list of its own, as Bazel's uploads go over up to
// `--remote_max_connections` and Nix's downloads over up to `http-connections`:
// https://github.com/bazelbuild/bazel/blob/a765dc177f48efb4c9e1b980609b838c9ba27f42/src/main/java/com/google/devtools/build/lib/remote/options/RemoteOptions.java#L65-L80
// https://github.com/NixOS/nix/blob/d1ecb78bf7a1c5604726bd72651bd2169c14163d/src/libstore/include/nix/store/filetransfer.hh#L52-L61
fn sent(
    reached: &Reached,
    streams: &[Vec<manifest::Piece>],
    sources: &BTreeMap<Sha256Digest, manifest::Source>,
) -> Result<Vec<Streamed>, String> {
    let began = Instant::now();
    let size_bytes = streams.iter().flatten().map(|piece| piece.size_bytes).sum();
    // Many streams write how many bytes they have sent, and this thread alone reads them; not
    // bounded, so that no stream waits while this thread opens connections, as the reads of the
    // pieces bound the counts.
    let (counting, counts) = mpsc::channel();
    thread::scope(|scope| {
        let (mut held, mut apart) = (BTreeSet::new(), true);
        let mut threads = Vec::new();
        let mut unstarted = None;
        for pieces in streams {
            let slot = if apart {
                reached.slot_apart(&held)
            } else {
                reached.slot()
            };
            let slot = match slot {
                Ok(slot) => slot,
                Err(error) => {
                    unstarted = Some(error);
                    break;
                }
            };
            let master = slot.master;
            // Once a stream shares a master, no other master is to be had.
            apart &= held.insert(master);
            let mut session = reached.session(&manifest::receive(), slot);
            let counting = counting.clone();
            let streaming = move || {
                // Its receiver reads until every stream has ended.
                let mut count = |bytes| counting.send(bytes).unwrap_or(());
                let (status, written) = streamed(&mut session, |stdin| {
                    manifest::stream(pieces, sources, stdin, &mut count)
                })?;
                drop(session);
                // Asked as soon as it ends, before another command could open its master again.
                let cut = reached.ended_connection(status, &[SSH_FAILED], master);
                Ok::<_, String>(Streamed {
                    master,
                    status,
                    cut,
                    written,
                })
            };
            let thread = (thread::Builder::new()).name(format!("stream over {master}"));
            match thread.spawn_scoped(scope, streaming) {
                Ok(thread) => threads.push(thread),
                Err(error) => {
                    unstarted = Some(format!(
                        "a thread to send a stream on can't be started: {error}"
                    ));
                    break;
                }
            }
        }
        drop(counting);
        said_progress(&reached.name, &counts, began, size_bytes);
        let mut ended = Vec::new();
        for thread in threads {
            ended.push(
                thread
                    .join()
                    .unwrap_or_else(|panic| panic::resume_unwind(panic))?,
            );
        }
        unstarted.map_or(Ok(ended), Err)
    })
}

fn said_progress(name: &Name, counts: &mpsc::Receiver<u64>, began: Instant, size_bytes: u64) {
    let (mut sent_bytes, mut next) = (0, began + PROGRESS);
    loop {
        let now = Instant::now();
        if now >= next {
            say(progress(name, sent_bytes, size_bytes, now - began));
            next = now + PROGRESS;
        }
        match counts.recv_timeout(next - now) {
            Ok(bytes) => sent_bytes += bytes,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// The blobs of `manifest` that the controller's store lacks.
fn lacked(
    reached: &Reached,
    manifest: &manifest::Manifest,
) -> Result<Vec<manifest::Missing>, String> {
    let (query, text) = (manifest::query(&manifest.name()), manifest.text());
    let asking = || reached.ssh(&query);
    let again = "asking again what it lacks";
    let answer = reached.again(
        asking,
        Some(&text),
        "ssh",
        &[SSH_FAILED],
        SHORT_TIMEOUT,
        again,
    )?;
    manifest::missing(&answer)
}

/// Runs `session` with a standard input that `write` fills, and returns how it ended with what
/// `write` returned. A stalled write stops the `ssh`.
fn streamed<T>(
    session: &mut Session,
    write: impl FnOnce(&mut Pipe) -> io::Result<T>,
) -> Result<(ExitStatus, io::Result<T>), String> {
    let child = session.stdin(Stdio::piped()).spawn();
    let mut child = child.context("running `ssh` failed")?;
    let Some(stdin) = child.stdin.take() else {
        unreachable!("the child's standard input is missing, though it is piped just above")
    };
    let mut pipe = Pipe(stdin);
    let written = write(&mut pipe);
    // Then dropped, so that the script reads its end.
    drop(pipe);
    if matches!(&written, Err(error) if error.kind() == io::ErrorKind::TimedOut) {
        child.kill().context("stopping `ssh` failed")?;
    }
    let status = child.wait().context("running `ssh` failed")?;
    Ok((status, written))
}

/// The standard input of an `ssh`, written only when it takes bytes: a write that finds no room
/// for `STALL_TIMEOUT` fails as `TimedOut`.
struct Pipe(ChildStdin);

impl io::Write for Pipe {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Ok(timeout) = Timespec::try_from(STALL_TIMEOUT) else {
            unreachable!("{STALL_TIMEOUT:?} fits a `timespec`")
        };
        loop {
            let mut polled = [PollFd::new(&self.0, PollFlags::OUT)];
            match event::poll(&mut polled, Some(&timeout)) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("it took no byte for {} s", STALL_TIMEOUT.as_secs()),
                    ));
                }
                Ok(_) => break,
                Err(Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
        // No more than a pipe that polls as writable takes at once, so that the write never waits.
        self.0.write(&bytes[..bytes.len().min(libc::PIPE_BUF)])
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

fn holds(name: &Name, manifest: &manifest::Manifest, streams: &[Vec<manifest::Piece>]) -> String {
    let pieces = || streams.iter().flatten();
    let lacked: BTreeSet<&Sha256Digest> = pieces().map(|piece| &piece.digest).collect();
    let files = manifest.entries();
    let files_count = files.len();
    if lacked.is_empty() {
        return format!("controller `{name}` holds all {files_count} of the task's files");
    }
    let lacked_count = files
        .iter()
        .filter(|file| lacked.contains(&file.digest))
        .count();
    let lacks = if lacked_count == files_count {
        format!("all {files_count} of the task's files")
    } else {
        format!("{lacked_count} of the task's {files_count} files")
    };
    let size_bytes = pieces().map(|piece| piece.size_bytes).sum();
    let over = match streams.len() {
        1 => String::new(),
        count => format!(" over {count} connections"),
    };
    format!(
        "controller `{name}` lacks {lacks}: sending {}{over}",
        size(size_bytes)
    )
}

// A count of what is done against the whole, as Modal says of the files of a mount it uploads:
// https://github.com/modal-labs/modal-client/blob/dbb24aba92e2c6c4fa8a123234813cfa48e09b0b/py/modal/mount.py#L597
fn progress(name: &Name, sent_bytes: u64, size_bytes: u64, elapsed: Duration) -> String {
    let rate = u128::from(sent_bytes) * 1000 / elapsed.as_millis().max(1);
    format!(
        "sent {} of {} to controller `{name}`, at {}/s",
        size(sent_bytes),
        size(size_bytes),
        size(u64::try_from(rate).unwrap_or(u64::MAX))
    )
}

fn size(bytes: u64) -> String {
    let units = [(1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")];
    let Some((unit, name)) = units.into_iter().find(|(unit, _)| bytes >= *unit) else {
        return crate::counted(bytes, "byte");
    };
    let tenths = bytes * 10 / unit;
    format!("{}.{} {name}", tenths / 10, tenths % 10)
}
