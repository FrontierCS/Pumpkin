//! Sending a run's or a build's job to the controller: the task's files, which go by content to
//! the controller's store, and the command that starts the job from them.
//!
//! The controller says which blobs of the task's manifest its store lacks, and only those are
//! sent, in streams over connections of their own. A connection that ends, or a stream that
//! stalls, costs only the pieces not yet stored: the send asks again what the controller lacks.
//!
//! The job's start then sends this machine's upstreams, their keys and its Modal settings into
//! the job's directory in tmpfs, as a `tar` on the connection's standard input, never in an
//! argument or in a file of this machine's, takes in what arrived, and starts the job in a unit of
//! the user's systemd, which outlives every connection.

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
use stdx::process::Environment;
use stdx::{Context as _, Deadline};

use super::{
    AGAIN, Name, Reached, SHORT_TIMEOUT, SSH_FAILED, STALL_TIMEOUT, Session, bounded, finished,
};
use crate::job::{SENT_CONFIG, SENT_MODAL, STARTED, STORE, TREES};
use crate::manifest::{self, INCOMING};
use crate::{config, next, say, workspace};

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
    /// Of the files git lists there, tracked, or untracked but not ignored, those it sends, and
    /// the git bundle of the Pumpkin that its `.pumpkinversion` names, which `./pumpkin` keeps once
    /// it has built it, from which the controller builds the same Pumpkin.
    hashed: manifest::Hashed,
    /// The entry's path below the repository's top.
    pub(super) entry: String,
    /// The repository's name, the top's own as `plain` gives it, which its trees on the
    /// controller are below.
    repo: String,
    commit: String,
    /// The task's workflow, as checking it gave it, which isn't sent.
    pub(super) workflow: Workflow,
}

/// Hashes what a run of `entry` needs of its git repository: the tracked and unignored files
/// that the task reaches, and what the controller builds Pumpkin with (`pumpkin`,
/// `.pumpkinversion` and the bundle of the commit that names).
///
/// # Errors
///
/// Fails if the entry isn't in a git repository, the repository has no `pumpkin` or
/// `.pumpkinversion` that names a commit with a bundle built here, or the task's root is above
/// the repository.
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
    if !top.join("pumpkin").is_file() {
        return Err(format!(
            "{} has no ./pumpkin, with which the controller builds the Pumpkin that \
             .pumpkinversion names: copy Pumpkin's deploy/pumpkin there",
            top.display()
        ));
    }
    let (commit, bundle) = pumpkin_version(&top)?;
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
    let hashed = manifest::hash(&top, &files, &commit, &bundle)?;
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
        commit,
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
/// that hold them, with `pumpkin` and `.pumpkinversion`, which the job runs.
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
    let mut kept: Vec<String> = reached.iter().map(|path| under(path)).collect();
    kept.extend(["pumpkin".to_owned(), ".pumpkinversion".to_owned()]);
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

/// The commit that `.pumpkinversion` names, and the path of the bundle of it that this machine
/// built.
fn pumpkin_version(top: &Path) -> Result<(String, PathBuf), String> {
    let path = top.join(".pumpkinversion");
    let text = fs::read_to_string(&path).context(path.display())?;
    let commit = text.split_whitespace().nth(1).unwrap_or_default();
    let hex = |c: char| c.is_ascii_digit() || ('a'..='f').contains(&c);
    if commit.len() != 40 || !commit.chars().all(hex) {
        return Err(format!(
            "{} names no commit after its tag: write the tag's full 40-digit commit",
            path.display()
        ));
    }
    let bundle = top.join(".pumpkin").join(commit).join("pumpkin.bundle");
    if !bundle.is_file() {
        return Err(format!(
            "{} isn't there, which the controller builds Pumpkin {commit} from: build it here \
             with `./pumpkin --version`, after removing {} if it was built without one",
            bundle.display(),
            top.join(".pumpkin").join(commit).display()
        ));
    }
    Ok((commit.to_owned(), bundle))
}

/// Sends the task's `tree` and the client's configuration, keys and Modal configuration to the
/// controller, and starts the job `id` of `kind` there, which runs `pumpkin` with `words`.
/// The keys go on the connection's standard input, to the job's directory in tmpfs, and are not
/// stored.
pub(super) fn send(
    reached: &Reached,
    client: &config::Client,
    id: &RunTag,
    kind: &str,
    tree: &Tree,
    words: &[String],
) -> Result<(), String> {
    let id = id.as_str();
    send_tree(reached, tree)?;
    let (config, keys) = client.sent();
    let config = toml::to_string(&config)
        .map_err(|error| format!("writing what the job is sent: {error}"))?;
    let mut files = vec![(SENT_CONFIG.to_owned(), config.into_bytes())];
    for (file, key) in keys {
        let read = fs::read(&key).map_err(|error| {
            format!(
                "the key file {} can't be read ({error}): put the key there",
                key.display()
            )
        })?;
        files.push((file, read));
    }
    if let Some(modal) = client.modal_file()? {
        files.push((SENT_MODAL.to_owned(), modal));
    }
    // Each name, `config.toml`, `modal.toml` or an upstream's name of at most 64 bytes with
    // `.key`, fits the 100 bytes of a name in the archive.
    let archive = manifest::archive(&files);
    let script = starting(&client.controller, id, kind, tree, words);
    let mut ssh = reached.ssh(&script)?;
    let output = ssh.output(Some(&archive), Deadline::after(SHORT_TIMEOUT));
    if let Ok(Some(output)) = &output
        && output.status.code() == Some(manifest::UNADMITTED)
    {
        return Err(manifest::unadmitted(&tree.hashed, &output.stdout));
    }
    let master = ssh.slot.master;
    finished(output, "ssh", SHORT_TIMEOUT)
        .map(|_| ())
        .map_err(|error| reached.lost(master, error))
}

/// The script that the controller runs to start the job: it unpacks the archive on its standard
/// input, admits the tree's blobs into the store, links the tree from them, with the bundle as
/// `<commit>.bundle` in the controller's one workspace, and starts the job's unit. It exits
/// [`manifest::UNADMITTED`] if a blob isn't whole.
fn starting(
    controller: &config::Controller,
    id: &str,
    kind: &str,
    tree: &Tree,
    words: &[String],
) -> String {
    let (repo, commit) = (&tree.repo, &tree.commit);
    let config::Controller {
        name,
        region,
        modal: _,
    } = controller;
    let words: Vec<String> = words.iter().map(|word| next::quoted(word)).collect();
    let (parameter, region) = (next::quoted(&parameter(name)), region.as_str());
    let manifest = &tree.hashed.manifest;
    let mut links = String::new();
    for entry in manifest.entries() {
        let manifest::Entry {
            kind,
            digest,
            size_bytes: _,
            path,
        } = entry;
        let (blob, to) = (format!("\"$store\"/{}", digest.hex()), next::quoted(path));
        let link = match kind {
            manifest::Kind::Bundle => {
                let bundle = format!("\"$HOME\"/{}/{path}.bundle", workspace::DIR);
                // The store's, in place of any other, as of one cut short by an earlier send;
                // linked aside and moved over it, as another job's start may link it at once, and
                // a job that reads it keeps what it opened.
                format!(
                    "if ! [ {bundle} -ef {blob} ]; then\n  ln --force {blob} {bundle}.$$\n  \
                     mv --force {bundle}.$$ {bundle}\nfi\n"
                )
            }
            _ if !STARTED.contains(&path.as_str()) => continue,
            manifest::Kind::File => format!("ln {blob} \"$tree\"/{to}\n"),
            // A link shares its file's mode, so an executable links to a copy of its own, 0555,
            // which another job's start may make at once, so it is made aside and moved in.
            manifest::Kind::Executable => format!(
                "[ -e {blob}.x ] || {{ copy=$(mktemp --tmpdir=\"$store\"/{INCOMING} \
                 executable.XXXXXX) && cp {blob} \"$copy\" && chmod 0555 \"$copy\" && \
                 mv \"$copy\" {blob}.x; }}\nln {blob}.x \"$tree\"/{to}\n"
            ),
            // With a `.` after it, so that `$(…)` keeps the target's own trailing newlines.
            manifest::Kind::Link => format!(
                "target=$(cat {blob} && echo .)\nln --symbolic -- \"${{target%.}}\" \"$tree\"/{to}\n"
            ),
        };
        links.push_str(&link);
    }
    format!(
        r#"set -o errexit -o nounset
sent="$XDG_RUNTIME_DIR/pumpkin/{id}"
tree="$HOME/{TREES}/{repo}/{id}"
store="$HOME/{STORE}"
mkdir --parents --mode=0700 "$XDG_RUNTIME_DIR/pumpkin" "$sent"
# Until the unit takes the keys, whose end removes them, a failure here does.
trap 'rm --recursive --force "$sent"' EXIT
tar --extract --directory="$sent"
(
{admit})
mkdir --parents "$tree" "$HOME/{kind}/{id}" "$HOME/{WORKSPACE}"
ln --symbolic "$HOME/{WORKSPACE}" "$tree/.pumpkin"
{links}systemd-run --user --quiet --expand-environment=no --unit=pumpkin-job-{id} \
    --working-directory="$tree" --property=ExitType=cgroup \
    --property="ExecStopPost=/usr/bin/rm --recursive --force $sent" \
    --property="StandardOutput=append:$HOME/{kind}/{id}/out" \
    --property="StandardError=append:$HOME/{kind}/{id}/log" \
    -- "$tree/pumpkin" --pumpkin-repository="$HOME/{WORKSPACE}/{commit}.bundle" \
    controller job {id} --region {region} --parameter {parameter} --tree {tree} -- {}
trap - EXIT
"#,
        words.join(" "),
        admit = manifest::admit(&manifest.name()),
        tree = manifest.name().hex(),
        WORKSPACE = workspace::DIR
    )
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
    let (bundles, files): (Vec<&manifest::Entry>, Vec<&manifest::Entry>) = (manifest.entries())
        .iter()
        .partition(|entry| entry.kind == manifest::Kind::Bundle);
    // A task's files are never none, as `pumpkin` and `.pumpkinversion` go with its own.
    let files_count = files.len();
    if lacked.is_empty() {
        return format!("controller `{name}` holds all {files_count} of the task's files");
    }
    let lacked_count = files
        .iter()
        .filter(|file| lacked.contains(&file.digest))
        .count();
    let mut lacks = Vec::new();
    if lacked_count == files_count {
        lacks.push(format!("all {files_count} of the task's files"));
    } else if lacked_count > 0 {
        lacks.push(format!("{lacked_count} of the task's {files_count} files"));
    }
    if bundles.iter().any(|bundle| lacked.contains(&bundle.digest)) {
        lacks.push("Pumpkin's bundle".to_owned());
    }
    let size_bytes = pieces().map(|piece| piece.size_bytes).sum();
    let over = match streams.len() {
        1 => String::new(),
        count => format!(" over {count} connections"),
    };
    format!(
        "controller `{name}` lacks {}: sending {}{over}",
        lacks.join(" and "),
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
