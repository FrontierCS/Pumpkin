//! A run's or a build's send of its job to the controller.
//!
//! A run or a build sends the files of its task's repository that the task reaches, and the
//! bundle of the Pumpkin it pins, by content, to the controller's store, which keeps each for
//! every later send, so that only what the store lacks goes, over several of the connections at
//! once ([`crate::manifest`]); then the job's start sends the upstreams of this machine's
//! configuration, their keys, its Modal provider and Modal's configuration file into its job's
//! directory in tmpfs, by `tar` on the connection's standard input, never in an argument nor in a
//! file of this machine's, takes what arrived into the store, and starts the job, `pumpkin
//! controller job`, which makes its tree of links into the store, in a unit of the user's
//! systemd, which outlives every connection.
//!
//! How a tree's files are read, dealt into streams and taken in there is [`crate::manifest`]'s;
//! following the job once it has started is the commands' ([`super::commands`]).

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
use stdx::process::Environment;
use stdx::{Context as _, Deadline};

use super::{
    AGAIN, Name, Reached, SHORT_TIMEOUT, SSH_FAILED, STALL_TIMEOUT, Session, bounded, finished,
};
use crate::job::{SENT_CONFIG, SENT_MODAL, STARTED, STORE, TREES};
use crate::manifest::{self, INCOMING};
use crate::{config, next, say, workspace};

/// How often a send says how much it has sent, so a send that ends sooner says nothing more.
// Often enough that a person sees a send move; seldom enough that a log of a send of minutes
// keeps a few lines of it. BuildKit's plain progress likewise says a step's count again only once
// it has moved 5%, or 5 s have passed:
// https://github.com/moby/buildkit/blob/1d510a3097d9fbd85a42d162d42c3a79eda1f795/util/progress/progressui/printer.go#L93-L101
const PROGRESS: Duration = Duration::from_secs(15);

/// The SSM parameter that holds the settings of the controller of `name`, as the module names
/// it.
fn parameter(name: &Name) -> String {
    format!("/pumpkin-{name}/settings")
}

/// A task's repository, as this machine sends it to the controller.
pub(super) struct Tree {
    /// Of the files git lists there, tracked, or untracked but not ignored, those it sends, and
    /// the git bundle of the Pumpkin that its `.pumpkinversion` names, which `./pumpkin` keeps
    /// once it has built it, from which the controller builds the same Pumpkin.
    hashed: manifest::Hashed,
    /// The entry's path below the repository's top.
    pub(super) entry: String,
    /// The repository's name, the top's own as [`plain`] gives it, which its trees on the
    /// controller are below.
    repo: String,
    /// The commit of Pumpkin that its `.pumpkinversion` names.
    commit: String,
}

/// The repository of the task whose entry is `entry`, given `args`, as the controller is sent it:
/// of the files that git lists there, those that checking the task reaches
/// ([`crate::star::reached`]), all that its build reads, with `./pumpkin` and `.pumpkinversion`.
///
/// # Errors
///
/// Fails if the entry is in no git repository, or the repository has no
/// `./pumpkin`, the consumer entry, which the job builds Pumpkin with, or a file to send can't be
/// sent ([`manifest::hash`]); or, saying them, if the task has problems.
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
    let files = needed(entry, args, &top, &files)?;
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
    })
}

/// `name` with each character but an ASCII letter or digit, `.`, `_` and `-` as `_`, as a name of a
/// file or directory.
fn plain(name: &str) -> String {
    let kept = |c: char| c.is_ascii_alphanumeric() || "._-".contains(c);
    name.chars()
        .map(|c| if kept(c) { c } else { '_' })
        .collect()
}

/// Of `listed`, the files that git lists in `top`, each ended by a NUL, those below it that
/// checking the task whose entry is `entry`, given `args`, reaches, as files or as directories
/// that hold them, with `./pumpkin` and `.pumpkinversion`, which the job runs.
fn needed(
    entry: &crate::star::Entry,
    args: &lang::Args,
    top: &Path,
    listed: &[u8],
) -> Result<Vec<u8>, String> {
    let (root, reached) = match crate::star::reached(entry, args)? {
        Ok(reached) => reached,
        Err(problems) => return Err(crate::task_problems(entry, &problems)),
    };
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

/// The commit that the `.pumpkinversion` in `top` names, after its tag, and the bundle of it that
/// `./pumpkin` keeps beside its build, `.pumpkin/<commit>/pumpkin.bundle`.
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

/// Sends `tree` to `reached` as job `id`'s, which runs `words`, `run` or `build` with their
/// arguments, in its directory below `kind`, with what `client` sends with each job, then starts
/// the job in a unit of its own, once the controller has taken in what it lacked of the tree.
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
    let (config, keys) = client.sent()?;
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
    let archive = archive(&files);
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

/// A `tar` archive, in POSIX's ustar format, of `files`, each a name and what the file holds,
/// each file 0600: what [`send`] sends a job, made in memory, so that what it holds of keys is
/// never written on this machine but in the files that hold them.
fn archive(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut archive = Vec::new();
    for (name, holds) in files {
        // The name, `config.toml`, `modal.toml` or an upstream's name of at most 64 bytes with
        // `.key`, fits its field's 100 bytes.
        archive.extend_from_slice(&manifest::header(name, 0o600, holds.len() as u64));
        archive.extend_from_slice(holds);
        archive.resize(archive.len().next_multiple_of(manifest::BLOCK), 0);
    }
    // Two blocks of zeros end it.
    archive.resize(archive.len() + 2 * manifest::BLOCK, 0);
    archive
}

/// What a command on `controller` runs to start job `id`, below `kind`, which runs `words` in
/// its tree, of `tree`'s manifest, building the Pumpkin it pins from its bundle, with what the job
/// is sent on its standard input, as `tar`: once it has taken in what the store lacked
/// ([`manifest::admit`]), it links into the tree what the job's unit runs ([`STARTED`]), and the
/// bundle into the controller's one workspace, as `<commit>.bundle`, in place of any there.
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
                // linked aside and moved over it, as another job's start may link it at once,
                // and a job that reads it keeps what it opened.
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

/// Sends `reached` what its store lacks of `tree`, saying what that is, or that it lacks
/// nothing: if it lacks anything, with the lock on sending it
/// ([`Shared::upload`](super::Shared::upload)), it asks again, and sends what it still lacks, over
/// several connections at once ([`sent`]); and once more, if a connection that went over ends, as
/// [`Reached::pause`] allows, as what has arrived stays there.
fn send_tree(reached: &Reached, tree: &Tree) -> Result<(), String> {
    let manifest::Hashed { manifest, sources } = &tree.hashed;
    if manifest::pieces(&lacked(reached, manifest)?).is_empty() {
        say(holds(&reached.name, manifest, &[]));
        return Ok(());
    }
    let _upload = reached.shared.upload(&reached.name)?;
    let mut attempt = 0;
    loop {
        let streams = manifest::deal(manifest::pieces(&lacked(reached, manifest)?));
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
                "a connection to controller `{}` ended at each of the send's {} attempts, the last \
                 as its log says: {}: run it again",
                reached.name,
                AGAIN.len() + 1,
                reached.shared.log(master).display()
            ));
        }
        attempt += 1;
    }
}

/// How a stream of [`sent`] ended: over shared master `master`, its `ssh`'s exit status, whether
/// that says its connection ended, and what [`manifest::stream`] gave.
struct Streamed {
    master: usize,
    status: ExitStatus,
    cut: bool,
    written: io::Result<Result<(), String>>,
}

impl Streamed {
    /// Whether the stream, whose connection didn't end, sent all its pieces to `reached`, else
    /// why not.
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

/// Sends `streams`, each a list of pieces read from where `sources` say, to `reached`, each a
/// [`manifest::receive`] of its own, at once, over a session of a master that no other of them
/// holds, as long as one can be had, else of one they hold ([`Reached::slot_apart`]): each starts
/// as its master opens. Says, each [`PROGRESS`] once all have started until the last has ended,
/// how much they have sent. Gives how each ended, once all have.
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
    // bounded, so that no stream waits while this thread opens masters, as the counts are bounded
    // by the reads of the pieces.
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

/// Says, each [`PROGRESS`] since `began` until every sender of `counts` has gone, how many of
/// `size_bytes` they have sent to controller `name`, as the counts of bytes they send add up.
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

/// What the store of `reached` lacks of the blobs that `manifest` names, as it answers
/// [`manifest::query`], asked again if the connection that went over ends.
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

/// Runs `session` with this machine's standard output and error, and its standard input written
/// by `write`, until it ends, or `write` fails as [`Pipe`] does once the session stalls, which
/// kills it; gives its exit status, and what `write` gave.
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

/// A command's standard input, which fails, as [`io::ErrorKind::TimedOut`], once it has taken no
/// byte for [`STALL_TIMEOUT`].
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

/// What controller `name` lacks of `manifest`'s blobs, for a person to read, as `streams` of
/// their pieces are to be sent: how many of the task's files, whether Pumpkin's bundle, and over
/// how many connections, if more than one; or that it holds them all.
fn holds(name: &Name, manifest: &manifest::Manifest, streams: &[Vec<manifest::Piece>]) -> String {
    let pieces = || streams.iter().flatten();
    let lacked: BTreeSet<&Sha256Digest> = pieces().map(|piece| &piece.digest).collect();
    let (bundles, files): (Vec<&manifest::Entry>, Vec<&manifest::Entry>) = (manifest.entries())
        .iter()
        .partition(|entry| entry.kind == manifest::Kind::Bundle);
    // A task's files are never one, as `./pumpkin` and `.pumpkinversion` go with its own.
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

/// What a send to controller `name` says of the pieces it sends, `size_bytes` in all, once it has
/// sent `sent_bytes` of them after `elapsed`: how much, and at what rate so far, for a person to
/// read.
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

/// `bytes` for a glance, to a tenth of the largest unit of 1024 that it reaches, as `3.4 MiB`.
fn size(bytes: u64) -> String {
    let units = [(1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")];
    let Some((unit, name)) = units.into_iter().find(|(unit, _)| bytes >= *unit) else {
        return crate::counted(bytes, "byte");
    };
    let tenths = bytes * 10 / unit;
    format!("{}.{} {name}", tenths / 10, tenths % 10)
}
