//! A link ([`crate::link`]) to the controller's `serve` of this build, over a session of a shared
//! master: the `serve` is uploaded there first if it has none, and a link that is cut is opened
//! again, by [`Reached::asked`] or by the caller.

use std::collections::BTreeSet;
use std::fs;
use std::num::NonZeroU32;
use std::path::PathBuf;

use schema::ir::Sha256Digest;
use stdx::Context as _;

use super::{OPENINGS, Reached, SHORT_TIMEOUT, Session, Slot};
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
    /// Each file's name in the install and where it is here.
    files: Vec<(&'static str, PathBuf)>,
    /// Each file's name in the install and its SHA-256.
    hashed: Vec<(&'static str, Sha256Digest)>,
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
        let hashed = (files.iter())
            .map(|(name, path)| Ok((*name, manifest::file_sha256(path)?.0)))
            .collect::<Result<Vec<_>, String>>()?;
        let key = link::key(&hashed);
        Ok(Self { files, hashed, key })
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
    /// Opens a link to the `serve` of `build` there, uploading `build`'s files first if the
    /// controller has none; an opening of a connection that doesn't get through is made up to
    /// `openings` times in all.
    pub(super) fn open(
        reached: &Reached,
        build: &Build,
        openings: NonZeroU32,
    ) -> Result<Self, Unlinked> {
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
        let asked = read(&mut self.link);
        asked.map_err(|unanswered| unlinked(reached, self.slot.master, unanswered))
    }

    /// `None` if the controller has no `serve` of `build`.
    fn connect(
        reached: &Reached,
        build: &Build,
        openings: NonZeroU32,
    ) -> Result<Option<Self>, Unlinked> {
        let slot = reached.slot_opening(&BTreeSet::new(), openings)?;
        let Session { command, slot } = reached.session(&link::command(&build.key), slot);
        match Link::open(command) {
            Ok(link) => Ok(link.map(|link| Self { link, slot })),
            Err(unanswered) => Err(unlinked(reached, slot.master, unanswered)),
        }
    }
}

impl Reached {
    /// What `ask` makes of a link to the `serve` of `build` there, opened as [`Linked::open`] opens
    /// one, and asked again over another, after each of `AGAIN`'s pauses, while one is cut before
    /// `ask` returns; `what` says what is asked, as in "listing the runs".
    pub(super) fn asked<T>(
        &self,
        build: &Build,
        what: &str,
        mut ask: impl FnMut(&mut Linked) -> Result<T, Unlinked>,
    ) -> Result<T, String> {
        self.carrying_on(what, |_| ask(&mut self.linked(build)?))
    }

    pub(super) fn linked(&self, build: &Build) -> Result<Linked, Unlinked> {
        Linked::open(self, build, OPENINGS)
    }

    /// As [`Self::asked`], with each attempt opening its link and reporting its progress.
    pub(super) fn carrying_on<T>(
        &self,
        what: &str,
        mut ask: impl FnMut(&mut dyn FnMut()) -> Result<T, Unlinked>,
    ) -> Result<T, String> {
        let mut attempt = 0;
        loop {
            let asked = ask(&mut || attempt = 0);
            let (master, said) = match asked {
                Ok(answer) => return Ok(answer),
                Err(Unlinked::Cut { master, said }) => (master, said),
                Err(Unlinked::Failed(error)) => return Err(error),
            };
            if !self.pause(attempt, master, &format!("{what} again")) {
                let failed = format!(
                    "{what} on controller `{}` failed, as `ssh` {said}",
                    self.name
                );
                return Err(self.lost(master, failed));
            }
            attempt += 1;
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

/// Uploads `build`'s files to the controller, as the install whose `serve` a link runs there.
fn install(reached: &Reached, build: &Build) -> Result<(), String> {
    let name = &reached.name;
    say(format_args!(
        "controller `{name}` has no serve of this pumpkin: installing it there"
    ));
    // The files are dropped once in the archive, so that the upload holds one copy of them.
    let archive = {
        let read = (build.files.iter())
            .map(|(name, path)| Ok(((*name).to_owned(), fs::read(path).context(path.display())?)))
            .collect::<Result<Vec<_>, String>>()?;
        manifest::archive(&read)
    };
    let mut session = reached.ssh(&link::installing(&build.hashed))?;
    let installed = reached.call(&mut session, Some(&archive), "ssh", SHORT_TIMEOUT);
    let installed = installed.map_err(|error| format!("installing it failed: {error}"));
    installed.map(drop)
}
