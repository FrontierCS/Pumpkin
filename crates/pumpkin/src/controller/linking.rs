//! One SSH connection to this build's `serve`. A command keeps it for all its requests.
//! An opening repairs the destination if needed, and a cut request is retried from its progress.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::time::Duration;
use std::{fs, thread};

use schema::ir::Sha256Digest;
use stdx::{Context as _, Deadline};

use super::{AGAIN, Found, Located, OPENINGS, Reached, SHORT_TIMEOUT, when};
use crate::link::{self, Link, Request, Unanswered};
use crate::{manifest, say, workspace};

// What `cargo xtask install` lays out beside `pumpkin` in `bin/` (its `built`, in
// `xtask/src/main.rs`, which this mirrors), of which a build may lack some.
const BESIDE: [&str; 5] = [
    "buildifier",
    "ec2-boot",
    "pumpkin-modal.py",
    "pumpkin-modal.py.lock",
    "sandboxd",
];

/// This build's files, which the controller installs whole and runs the `pumpkin` of as `serve`:
/// its executable, and whichever of `BESIDE` lie beside it, as they are, so that a `pumpkin` run
/// from cargo's `target/debug` brings that directory's debug builds.
pub(super) struct Build {
    pub(super) hashed: manifest::Hashed,
    key: Sha256Digest,
}

impl Build {
    /// Hashes this build's files; `expected` ends the message of a failure to find its executable.
    pub(super) fn of_this(expected: &str) -> Result<Self, String> {
        let exe = workspace::exe(expected)?;
        let mut files = vec![("pumpkin", exe.clone())];
        for name in BESIDE {
            let path = exe.with_file_name(name);
            if path.try_exists().context(path.display())? {
                files.push((name, path));
            }
        }
        let mut entries = Vec::new();
        let mut sources = std::collections::BTreeMap::new();
        for (name, path) in files {
            let (digest, size_bytes) = manifest::file_sha256(&path)?;
            let kind = match name {
                "pumpkin-modal.py" | "pumpkin-modal.py.lock" => manifest::Kind::File,
                _ => manifest::Kind::Executable,
            };
            entries.push(manifest::Entry {
                kind,
                digest: digest.clone(),
                size_bytes,
                path: name.to_owned(),
            });
            sources.insert(digest, manifest::Source::File(path));
        }
        let manifest = manifest::Manifest::new(entries);
        let key = manifest.name();
        let hashed = manifest::Hashed { manifest, sources };
        Ok(Self { hashed, key })
    }
}

pub(super) enum Unlinked {
    Cut { said: String, denied: bool },
    Failed(String),
}

impl From<String> for Unlinked {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}

pub(super) struct Linked {
    link: Link,
    // OpenSSH puts only its own diagnostics here. The remote command's stderr stays the user's.
    diagnostics: tempfile::NamedTempFile,
}

impl Linked {
    pub(super) fn open(reached: &Reached, build: &Build, at: &Located) -> Result<Self, Unlinked> {
        if let Some(linked) = Self::connect(reached, build, at)? {
            return Ok(linked);
        }
        install(reached, build, at)?;
        Self::connect(reached, build, at)?.ok_or_else(|| {
            Unlinked::Failed(format!(
                "controller `{}` has no serve of this pumpkin though it was just installed \
                 there, in `~/{}/{}`: see that its user may run it, and run the command again",
                reached.name,
                link::installs(),
                build.key.hex()
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
        self.stream(reached, |link| link.ask(request, body))
    }

    pub(super) fn stream<T>(
        &mut self,
        reached: &Reached,
        read: impl FnOnce(&mut Link) -> Result<T, Unanswered>,
    ) -> Result<T, Unlinked> {
        read(&mut self.link).map_err(|error| unlinked(reached, &self.diagnostics, error))
    }

    fn connect(reached: &Reached, build: &Build, at: &Located) -> Result<Option<Self>, Unlinked> {
        let diagnostics = tempfile::NamedTempFile::new()
            .context("creating a temporary file for SSH diagnostics failed")?;
        let mut ssh = Reached::ssh_command();
        ssh.arg("-E").arg(diagnostics.path());
        reached.remote(&mut ssh, at, &link::command(&build.key));
        match Link::open(ssh) {
            Ok(link) => Ok(link.map(|link| Self { link, diagnostics })),
            Err(error) => Err(unlinked(reached, &diagnostics, error)),
        }
    }
}

impl Reached {
    fn connected(&self, build: &Build, mut found: Found) -> Result<(Linked, Found), String> {
        let mut repaired = false;
        let mut enrolled = false;
        for attempt in 0..OPENINGS {
            let (said, denied) = match Linked::open(self, build, &found.at) {
                Ok(link) => return Ok((link, found)),
                Err(Unlinked::Failed(error)) => return Err(error),
                Err(Unlinked::Cut { said, denied }) => (said, denied),
            };
            if denied && !enrolled || !repaired && attempt > 0 {
                found = self.repair(&found, denied, &said)?;
                repaired = true;
                enrolled |= denied;
            } else if denied || attempt + 1 == OPENINGS {
                self.location.failed(&self.name, &found, &said)?;
                return Err(said);
            }
            let pause = Duration::from_secs(((1_u64 << attempt) - 1).min(30));
            say(format_args!(
                "{said}: opening it again {}, attempt {} of {OPENINGS}",
                when(pause),
                attempt + 2
            ));
            thread::sleep(pause);
        }
        unreachable!("the last opening returns above")
    }

    /// Opens the first link of a command, starting the controller if its opening needs repair.
    pub(super) fn opening(&self, build: &Build) -> Result<(Linked, Found), String> {
        self.connected(build, self.found()?)
    }

    /// What `ask` makes of a link to the `serve` of `build` there, opened as [`Self::opening`] opens
    /// one, and asked again over another, after each of `AGAIN`'s pauses, while one is cut before
    /// `ask` returns; `what` says what is asked, as in "listing the runs".
    pub(super) fn asked<T>(
        &mut self,
        build: &Build,
        what: &str,
        mut ask: impl FnMut(&Self, &mut Linked) -> Result<T, Unlinked>,
    ) -> Result<T, String> {
        self.carrying_on(build, what, |reached, linked, _| ask(reached, linked))
    }

    /// As [`Self::asked`], with the pauses starting over each time `ask` reports progress.
    pub(super) fn carrying_on<T>(
        &mut self,
        build: &Build,
        what: &str,
        mut ask: impl FnMut(&Self, &mut Linked, &mut dyn FnMut()) -> Result<T, Unlinked>,
    ) -> Result<T, String> {
        let mut attempt = 0;
        loop {
            let (mut linked, found) = match self.linked.take() {
                Some(link) => link,
                None => self.opening(build)?,
            };
            let error = match ask(self, &mut linked, &mut || attempt = 0) {
                Ok(answer) => {
                    self.linked = Some((linked, found));
                    return Ok(answer);
                }
                Err(Unlinked::Cut { said, .. }) => said,
                Err(Unlinked::Failed(error)) => return Err(error),
            };
            drop(linked);
            let Some(pause) = AGAIN.get(attempt) else {
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

fn unlinked(
    reached: &Reached,
    diagnostics: &tempfile::NamedTempFile,
    error: Unanswered,
) -> Unlinked {
    let name = &reached.name;
    match error {
        Unanswered::Unreached(ended) => {
            let line = last_line(diagnostics);
            let denied = line
                .as_ref()
                .is_some_and(|line| line.contains("Permission denied ("));
            let said = line.map_or(ended, |line| format!("ssh says: {line}"));
            Unlinked::Cut {
                said: format!("controller `{name}` didn't answer: {said}"),
                denied,
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

/// Uploads `build`'s files to the controller, as the install whose `serve` a link runs there.
fn install(reached: &Reached, build: &Build, at: &Located) -> Result<(), Unlinked> {
    let name = &reached.name;
    say(format_args!(
        "controller `{name}` has no serve of this pumpkin: installing it there"
    ));
    // The files are dropped once in the archive, so that the upload holds one copy of them.
    let archive = {
        let read = build
            .hashed
            .manifest
            .entries()
            .iter()
            .map(|entry| {
                let Some(manifest::Source::File(path)) = build.hashed.sources.get(&entry.digest)
                else {
                    unreachable!("each install entry was hashed from a file")
                };
                Ok((entry.path.clone(), fs::read(path).context(path.display())?))
            })
            .collect::<Result<Vec<_>, String>>()?;
        manifest::archive(&read)
    };
    let diagnostics = tempfile::NamedTempFile::new()
        .context("creating a temporary file for SSH diagnostics failed")?;
    let mut ssh = Reached::ssh_command();
    ssh.arg("-E").arg(diagnostics.path());
    reached.remote(&mut ssh, at, &link::installing(&build.hashed.manifest));
    let installed = stdx::process::fed(&mut ssh, &archive, Deadline::after(SHORT_TIMEOUT));
    match installed {
        Ok(Some(result)) if result.status.success() => Ok(()),
        Ok(Some(result)) if !result.stderr.is_empty() => Err(Unlinked::Failed(format!(
            "installing serve failed: {}",
            String::from_utf8_lossy(&result.stderr).trim_end()
        ))),
        Ok(None) => Err(Unlinked::Failed(format!(
            "installing serve timed out after {} s",
            SHORT_TIMEOUT.as_secs()
        ))),
        Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => Err(Unlinked::Failed(
            format!("installing serve failed: {error}"),
        )),
        Ok(Some(_)) | Err(_) => Err(unlinked(
            reached,
            &diagnostics,
            Unanswered::Unreached("ended during the install".to_owned()),
        )),
    }
}
