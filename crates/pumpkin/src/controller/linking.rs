//! A link ([`crate::link`]) to the controller's `serve` of this build, over a session of a shared
//! master: the `serve` is uploaded there first if it has none, and a link that is cut is the
//! caller's to open again, as only what reads goes over it.

use std::fs;
use std::path::PathBuf;

use schema::ir::Sha256Digest;
use stdx::Context as _;

use super::{Name, Reached, SHORT_TIMEOUT, Session, Slot};
use crate::link::{self, Answer, Ask, Health, Link, Listed, Unanswered};
use crate::{manifest, say, workspace};

/// This build's executable, which the controller runs as `serve`, and its SHA-256.
pub(super) struct Build {
    exe: PathBuf,
    digest: Sha256Digest,
}

impl Build {
    /// Hashes this executable; `expected` ends the message of a failure to find it.
    pub(super) fn of_this(expected: &str) -> Result<Self, String> {
        let exe = workspace::exe(expected)?;
        let (digest, _) = manifest::file_sha256(&exe)?;
        Ok(Self { exe, digest })
    }
}

/// Why no answer came over a link.
pub(super) enum Unlinked {
    /// The link, over connection `master`, ended before what was asked for was whole, as `ssh`
    /// says, as when the connection is cut: asking again, over another, may answer.
    Cut { master: usize, said: String },
    /// Asking again would fail again: why, with what was being done.
    Failed(String),
}

impl From<String> for Unlinked {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}

/// A link to the controller's `serve`, held, with its session, until it is dropped.
pub(super) struct Linked {
    // Before the slot, so that the link's `ssh` ends before its session is given up.
    link: Link,
    master: usize,
    _slot: Slot,
    name: Name,
}

impl Linked {
    /// Opens a link to the `serve` of `build` there, uploading `build`'s executable first if the
    /// controller has none.
    pub(super) fn open(reached: &Reached, build: &Build) -> Result<Self, Unlinked> {
        if let Some(linked) = Self::connect(reached, build)? {
            return Ok(linked);
        }
        install(reached, build)?;
        Self::connect(reached, build)?.ok_or_else(|| {
            Unlinked::Failed(format!(
                "controller `{}` has no serve of this pumpkin though it was just installed \
                 there, in `~/{}/{}`: see that its user may run it, and run the command again",
                reached.name,
                link::installs(),
                build.digest.hex()
            ))
        })
    }

    /// The controller's health, or the message of its refusal.
    pub(super) fn health(&mut self) -> Result<Result<Health, String>, Unlinked> {
        let asked = self.link.health();
        asked.map_err(|unanswered| unlinked(&self.name, self.master, unanswered))
    }

    /// The runs whose ids start with one of `prefixes`, or the message of the refusal.
    pub(super) fn list(&mut self, prefixes: &[String]) -> Result<Result<Listed, String>, Unlinked> {
        let asked = self.link.list(prefixes);
        asked.map_err(|unanswered| unlinked(&self.name, self.master, unanswered))
    }

    /// The answer to `ask`.
    pub(super) fn ask(&mut self, ask: &Ask) -> Result<Answer, Unlinked> {
        let asked = self.link.ask(ask);
        asked.map_err(|unanswered| unlinked(&self.name, self.master, unanswered))
    }

    /// `None` if the controller has no `serve` of `build`.
    fn connect(reached: &Reached, build: &Build) -> Result<Option<Self>, Unlinked> {
        let Session { command, slot } = reached.ssh(&link::command(&build.digest))?;
        let master = slot.master;
        match Link::open(command) {
            Ok(link) => Ok(link.map(|link| Self {
                link,
                master,
                _slot: slot,
                name: reached.name.clone(),
            })),
            Err(unanswered) => Err(unlinked(&reached.name, master, unanswered)),
        }
    }
}

fn unlinked(name: &Name, master: usize, unanswered: Unanswered) -> Unlinked {
    match unanswered {
        Unanswered::Unreached(said) => Unlinked::Cut { master, said },
        Unanswered::Failed(error) => {
            Unlinked::Failed(format!("linking to controller `{name}` failed: {error}"))
        }
    }
}

/// Uploads `build`'s executable to the controller, as the `serve` that a link runs there.
fn install(reached: &Reached, build: &Build) -> Result<(), String> {
    let name = &reached.name;
    say(format_args!(
        "controller `{name}` has no serve of this pumpkin: installing it there"
    ));
    let bytes = fs::read(&build.exe).context(build.exe.display())?;
    let mut session = reached.ssh(&link::installing(&build.digest))?;
    let installed = reached.call(&mut session, Some(&bytes), "ssh", SHORT_TIMEOUT);
    let installed = installed.map_err(|error| format!("installing it failed: {error}"));
    installed.map(drop)
}
