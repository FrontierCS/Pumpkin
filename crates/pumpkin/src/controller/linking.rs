//! A link ([`crate::link`]) to the controller's `serve` of this build, over a session of a shared
//! master: the `serve` is uploaded there first if it has none, and a link that is cut is the
//! caller's to open again, as only what reads goes over it.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use schema::ir::Sha256Digest;
use stdx::Context as _;

use super::{Reached, SHORT_TIMEOUT, Session, Slot};
use crate::link::{self, Health, Link, Listed, Unanswered, Viewed};
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
    /// The link, over connection `master`, ended before what was asked for was whole, and the
    /// connection no longer answers, as when it is cut: asking again, over another, may answer.
    /// `said` is how the frames ended, in Pumpkin's words.
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
    slot: Slot,
}

impl Linked {
    /// Opens a link to the `serve` of `build` there, uploading `build`'s executable first if the
    /// controller has none; an opening of a connection that doesn't get through is made up to
    /// `openings` times in all.
    pub(super) fn open(reached: &Reached, build: &Build, openings: u32) -> Result<Self, Unlinked> {
        if let Some(linked) = Self::connect(reached, build, openings)? {
            return Ok(linked);
        }
        install(reached, build)?;
        Self::connect(reached, build, openings)?.ok_or_else(|| {
            Unlinked::Failed(format!(
                "controller `{}` has no serve of this pumpkin though it was just installed \
                 there, in `~/{}/{}`: see that its user may run it, and run the command again",
                reached.name,
                link::installs(),
                build.digest.hex()
            ))
        })
    }

    /// The controller's health, or the message of its refusal: `reached` is the controller that
    /// the link goes to.
    pub(super) fn health(&mut self, reached: &Reached) -> Result<Result<Health, String>, Unlinked> {
        let asked = self.link.health();
        asked.map_err(|unanswered| unlinked(reached, self.slot.master, unanswered))
    }

    /// The runs whose ids start with one of `prefixes`, or the message of the refusal, as
    /// [`Linked::health`] says.
    pub(super) fn list(
        &mut self,
        reached: &Reached,
        prefixes: &[String],
    ) -> Result<Result<Listed, String>, Unlinked> {
        let asked = self.link.list(prefixes);
        asked.map_err(|unanswered| unlinked(reached, self.slot.master, unanswered))
    }

    /// The view's reply to a `GET` of `target`, or the message of the refusal, as
    /// [`Linked::health`] says.
    pub(super) fn view(
        &mut self,
        reached: &Reached,
        target: &str,
        deflate: bool,
    ) -> Result<Result<Viewed, String>, Unlinked> {
        let asked = self.link.view(target, deflate);
        asked.map_err(|unanswered| unlinked(reached, self.slot.master, unanswered))
    }

    /// `None` if the controller has no `serve` of `build`.
    fn connect(reached: &Reached, build: &Build, openings: u32) -> Result<Option<Self>, Unlinked> {
        let slot = reached.slot_opening(&BTreeSet::new(), openings)?;
        let Session { command, slot } = reached.session(&link::command(&build.digest), slot);
        match Link::open(command) {
            Ok(link) => Ok(link.map(|link| Self { link, slot })),
            Err(unanswered) => Err(unlinked(reached, slot.master, unanswered)),
        }
    }
}

fn unlinked(reached: &Reached, master: usize, unanswered: Unanswered) -> Unlinked {
    let name = &reached.name;
    match unanswered {
        // A `serve` that ended on its own, with the connection up, would end the same way again.
        Unanswered::Unreached(said) if !reached.shared.answers(master, &reached.host()) => {
            Unlinked::Cut { master, said }
        }
        Unanswered::Unreached(said) => Unlinked::Failed(format!(
            "linking to controller `{name}` failed: its `serve` ended before it answered in \
             full (the link {said}): what it printed above says why"
        )),
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
