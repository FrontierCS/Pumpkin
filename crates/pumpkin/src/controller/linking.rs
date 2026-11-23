//! Direct SSH connections to this build's `serve`. A command keeps its first for its requests.
//! An opening repairs the destination if needed, and a cut request is retried from its progress.

use std::collections::BTreeMap;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use schema::ir::Sha256Digest;
use stdx::Context as _;
use stdx::process::{Bound, Echo, Failure, Run};

use super::{AGAIN, Found, Located, OPENINGS, Reached, SHORT_TIMEOUT, when};
use crate::link::{self, Link, Request, Unanswered};
use crate::{manifest, say, workspace};

// The most of what a command there wrote to its standard error that a failed link says: enough
// for a clap usage error or a panic's message. Kubernetes bounds a response's warnings so:
// https://github.com/kubernetes/kubernetes/blob/ab69524f795c42094a6630298ff53f3c3ebab7f4/staging/src/k8s.io/apiserver/pkg/endpoints/filters/warning.go#L39-L42
const REMOTE_TAIL_BYTES: u64 = 4096;

/// This build's files, which the controller installs whole. The controller runs their `pumpkin`
/// as `serve`. They are this build's executable, and whichever of an install's other files lie
/// beside it, as they are. They omit the install's links, which a client never sends. So a
/// `pumpkin` run from cargo's `target/debug` brings that directory's debug builds.
pub(super) struct Build {
    files: Vec<(&'static str, PathBuf)>,
    pub(super) hashed: manifest::Hashed,
}

impl Build {
    /// Hashes this build's files; `expected` ends the message of a failure to find its executable.
    pub(super) fn of_this(expected: &str) -> Result<Self, String> {
        let exe = workspace::exe(expected)?;
        let mut files = Vec::new();
        let mut entries = Vec::new();
        let mut sources = BTreeMap::new();
        for file in install::File::ALL
            .into_iter()
            .filter(|file| !file.is_link())
        {
            let own = file == install::File::Pumpkin;
            let path = if own {
                exe.clone()
            } else {
                exe.with_file_name(file.name())
            };
            if !own && !path.try_exists().context(path.display())? {
                continue;
            }
            let (digest, size_bytes) = manifest::file_sha256(&path)?;
            let kind = if file.is_executable() {
                manifest::Kind::Executable
            } else {
                manifest::Kind::File
            };
            entries.push(manifest::Entry {
                kind,
                digest: digest.clone(),
                size_bytes,
                path: file.name().to_owned(),
            });
            sources.insert(digest, manifest::Source::File(path.clone()));
            files.push((file.name(), path));
        }
        files.sort_unstable_by_key(|&(name, _)| name);
        let manifest = manifest::Manifest::new(entries);
        let hashed = manifest::Hashed { manifest, sources };
        Ok(Self { files, hashed })
    }
}

pub(super) enum Unlinked {
    Cut { said: String, denied: bool },
    HostKey(String),
    Failed(String),
}

impl From<String> for Unlinked {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}

pub(super) struct Linked {
    link: Link,
    // OpenSSH puts only its own diagnostics here.
    diagnostics: tempfile::NamedTempFile,
    // What the command there writes to its standard error, which `ssh` writes to its own: of
    // `serve`, only its error as it ends, and that it can't mark its install.
    remote: tempfile::NamedTempFile,
}

impl Linked {
    pub(super) fn open(reached: &Reached, build: &Build, at: &Located) -> Result<Self, Unlinked> {
        let key = build.hashed.manifest.name();
        if let Some(linked) = Self::connect(reached, &key, at)? {
            return Ok(linked);
        }
        install(reached, build, at)?;
        Self::connect(reached, &key, at)?.ok_or_else(|| {
            Unlinked::Failed(format!(
                "controller `{}` has no serve of this pumpkin though it was just installed \
                 there, in `~/{}/bin`: see that its user may run it, and run the command again",
                reached.name,
                link::install(&key),
            ))
        })
    }
    /// The answer to `request`, or the message of its refusal: `reached` is the controller that
    /// the link goes to.
    pub(super) fn ask<R: Request>(
        &mut self,
        reached: &Reached,
        request: R,
    ) -> Result<Result<R::Answer, String>, Unlinked> {
        self.ask_with_body(reached, request, &[])
    }

    pub(super) fn ask_with_body<R: Request>(
        &mut self,
        reached: &Reached,
        request: R,
        body: &[u8],
    ) -> Result<Result<R::Answer, String>, Unlinked> {
        self.stream(reached, |link| {
            link.send(&request.ask(), body)?;
            link.answer::<R>()
        })
    }

    pub(super) fn stream<T>(
        &mut self,
        reached: &Reached,
        read: impl FnOnce(&mut Link) -> Result<T, Unanswered>,
    ) -> Result<T, Unlinked> {
        read(&mut self.link)
            .map_err(|error| unlinked(reached, &self.diagnostics, Some(&self.remote), error))
    }

    fn connect(
        reached: &Reached,
        key: &Sha256Digest,
        at: &Located,
    ) -> Result<Option<Self>, Unlinked> {
        let diagnostics = reached.location.diagnostics()?;
        let remote = reached.location.diagnostics()?;
        // Never this process's own standard error. OpenSSH makes each of its standard streams that
        // isn't a terminal non-blocking, and blocking again only as it closes it, which an `ssh`
        // killed, or ended by a fatal error, never does. A write to a stream left so fails with
        // `WouldBlock`, in each process that shares it, as under `2>&1`.
        // https://github.com/openssh/openssh-portable/blob/5dba72adacbd39736960543373737bc01e35f778/ssh.c#L2234-L2251
        // https://github.com/openssh/openssh-portable/blob/5dba72adacbd39736960543373737bc01e35f778/channels.c#L514-L540
        // https://github.com/openssh/openssh-portable/blob/5dba72adacbd39736960543373737bc01e35f778/channels.c#L636-L645
        // https://github.com/openssh/openssh-portable/blob/5dba72adacbd39736960543373737bc01e35f778/clientloop.c#L2901-L2908
        let stderr = remote.as_file().try_clone();
        let stderr = stderr.context(remote.path().display())?;
        let mut ssh = Reached::ssh_command();
        ssh.arg("-E").arg(diagnostics.path());
        reached.remote(&mut ssh, at, &link::command(key));
        match Link::open(ssh, stderr) {
            Ok(link) => Ok(link.map(|link| Self {
                link,
                diagnostics,
                remote,
            })),
            Err(error) => Err(unlinked(reached, &diagnostics, Some(&remote), error)),
        }
    }
}

impl Reached {
    fn connected(&self, build: &Build, mut found: Found) -> Result<(Linked, Found), String> {
        let mut repaired = false;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let said = match Linked::open(self, build, &found.at) {
                Ok(link) => return Ok((link, found)),
                Err(Unlinked::Failed(error)) => return Err(error),
                Err(Unlinked::HostKey(error)) => {
                    if !repaired && attempt < OPENINGS {
                        // The cached address may now belong to a different host. Check the pin
                        // against the discovered address before reporting a change there.
                        found = self.repair(&found, &error)?;
                        repaired = true;
                        continue;
                    }
                    self.location.failed(&self.name, &found, &error)?;
                    return Err(error);
                }
                // The controller refuses a key that its admin hasn't listed. No repair or retry
                // changes that.
                Err(Unlinked::Cut { denied: true, .. }) => return Err(self.key_refused()?),
                Err(Unlinked::Cut {
                    said,
                    denied: false,
                }) => said,
            };
            if attempt == OPENINGS {
                let hosts = self.kept.join("known_hosts");
                let error = format!(
                    "{said}: check SSH access to the controller and the host key pinned for `{}` in {}, then run the command again",
                    self.alias,
                    hosts.display()
                );
                self.location.failed(&self.name, &found, &error)?;
                return Err(error);
            }
            if !repaired && attempt > 1 {
                found = self.repair(&found, &said)?;
                repaired = true;
            }
            let pause = Duration::from_secs(((1_u64 << (attempt - 1)) - 1).min(30));
            say(format_args!(
                "{said}: opening it again {}, attempt {} of {OPENINGS}",
                when(pause),
                attempt + 1
            ));
            thread::sleep(pause);
        }
    }

    /// Opens the first link of a command, starting the controller if its opening needs repair.
    pub(super) fn opening(&self, build: &Build) -> Result<(Linked, Found), String> {
        self.connected(build, self.found()?)
    }

    /// Keeps a link to this build across requests, asking again after a cut. `what` names the
    /// operation, as in "listing the runs".
    pub(super) fn asked<T>(
        &mut self,
        build: &Build,
        what: &str,
        mut ask: impl FnMut(&Self, &mut Linked) -> Result<T, Unlinked>,
    ) -> Result<T, String> {
        self.carrying_on(build, what, |reached, linked, _, _| ask(reached, linked))
    }

    /// As [`Self::asked`], with each attempt opening its link and reporting its progress.
    pub(super) fn carrying_on<T>(
        &mut self,
        build: &Build,
        what: &str,
        mut ask: impl FnMut(
            &Self,
            &mut Linked,
            &Located,
            &mut (dyn FnMut() + Send),
        ) -> Result<T, Unlinked>,
    ) -> Result<T, String> {
        let mut attempt = 0;
        loop {
            let (mut linked, found) = match self.linked.take() {
                Some(link) => link,
                None => self.opening(build)?,
            };
            let error = match ask(self, &mut linked, &found.at, &mut || attempt = 0) {
                Ok(answer) => {
                    self.linked = Some((linked, found));
                    return Ok(answer);
                }
                Err(Unlinked::Cut { said, .. }) => said,
                Err(Unlinked::HostKey(error) | Unlinked::Failed(error)) => return Err(error),
            };
            drop(linked);
            let Some(pause) = AGAIN.get(attempt) else {
                let error =
                    format!("{error}: run the command again when the controller is reachable");
                self.location.failed(&self.name, &found, &error)?;
                return Err(error);
            };
            attempt += 1;
            say(format_args!(
                "{error}: {what} again {}, attempt {} of {}",
                when(*pause),
                attempt + 1,
                AGAIN.len() + 1
            ));
            thread::sleep(*pause);
        }
    }
}

/// Why a link to `reached` gave no answer, `error`, with the last line of `ssh`'s `diagnostics`.
/// Says first what the command there wrote to its standard error, `remote`, if anything.
fn unlinked(
    reached: &Reached,
    diagnostics: &tempfile::NamedTempFile,
    remote: Option<&tempfile::NamedTempFile>,
    error: Unanswered,
) -> Unlinked {
    let name = &reached.name;
    if let Some(said) = remote.and_then(said_there) {
        say(format_args!(
            "controller `{name}` wrote to its standard error:\n{said}"
        ));
    }
    match error {
        Unanswered::Unreached(ended) => {
            let line = last_line(diagnostics);
            // OpenSSH's fatal when the server's host key cannot be verified.
            // https://github.com/openssh/openssh-portable/blob/5dba72adacbd39736960543373737bc01e35f778/sshconnect2.c#L104
            let host_refused = line.as_deref() == Some("Host key verification failed.");
            // OpenSSH's fatal when the server refuses every key.
            // https://github.com/openssh/openssh-portable/blob/5dba72adacbd39736960543373737bc01e35f778/sshconnect2.c#L543
            let denied = line
                .as_ref()
                .is_some_and(|line| line.contains(": Permission denied (") && line.ends_with(")."));
            let said = line.map_or(ended, |line| format!("ssh says: {line}"));
            let said = format!("controller `{name}` didn't answer: {said}");
            if host_refused {
                Unlinked::HostKey(reached.host_refused(&said))
            } else {
                Unlinked::Cut { said, denied }
            }
        }
        Unanswered::Failed(error) => {
            Unlinked::Failed(format!("linking to controller `{name}` failed: {error}"))
        }
    }
}

fn last_line(file: &tempfile::NamedTempFile) -> Option<String> {
    // OpenSSH's MSGBUFSIZ is 1024. Leave room for the timestamp and a line ending.
    const TAIL_BYTES: u64 = 2048;
    let mut file = file.reopen().ok()?;
    let size = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(size.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    String::from_utf8_lossy(&bytes)
        .lines()
        .last()
        .map(str::to_owned)
}

/// The last lines of `remote`, a command's standard error, each escaped and indented, as many as
/// fit [`REMOTE_TAIL_BYTES`]; `None` if it holds none or can't be read.
fn said_there(remote: &tempfile::NamedTempFile) -> Option<String> {
    let mut file = remote.reopen().ok()?;
    let size = file.metadata().ok()?.len();
    let from = size.saturating_sub(REMOTE_TAIL_BYTES);
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut bytes = Vec::new();
    file.take(REMOTE_TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    // A tail that starts within a line leaves out the part of it.
    if from > 0 {
        lines.next();
    }
    let lines: Vec<String> = (lines.filter(|line| !line.is_empty()))
        .map(|line| format!("  {}", crate::escaped(line)))
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// Uploads `build`'s files to the controller, as the install whose `serve` a link runs there.
fn install(reached: &Reached, build: &Build, at: &Located) -> Result<(), Unlinked> {
    let name = &reached.name;
    say(format_args!(
        "controller `{name}` has no serve of this pumpkin: installing it there"
    ));
    let archive = manifest::archive(&build.files)?;
    let diagnostics = reached.location.diagnostics()?;
    let mut ssh = Reached::ssh_command();
    ssh.arg("-E").arg(diagnostics.path());
    reached.remote(&mut ssh, at, &link::installing(&build.hashed.manifest));
    let how = Run::new(Bound::Within(SHORT_TIMEOUT), Echo::Silent).input(&archive);
    match stdx::process::run(&mut ssh, how) {
        Ok(_) => Ok(()),
        // Only the install writes to standard error, as `ssh` writes its diagnostics to a file.
        Err(failure @ Failure::Exited { .. }) if !failure.reported().is_empty() => Err(
            Unlinked::Failed(format!("installing serve failed: {}", failure.unadvised())),
        ),
        Err(Failure::Exited { .. }) => Err(unlinked(
            reached,
            &diagnostics,
            None,
            Unanswered::Unreached("ended during the install".to_owned()),
        )),
        Err(
            failure @ (Failure::Unstarted { .. }
            | Failure::Unwatched { .. }
            | Failure::Unended { .. }),
        ) => Err(Unlinked::Failed(format!(
            "installing serve failed: {failure}"
        ))),
    }
}
