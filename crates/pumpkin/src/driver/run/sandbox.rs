//! The runtime's one start path, which every sandbox of a run starts on ([`Sandboxes`]), and
//! what each role's sandbox needs of it: its IR command launched, and what it wrote collected.
//!
//! The provider's side of each step is [`Provider`]'s, and the session's [`Session`]'s.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, DirBuilder};
use std::iter;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use pumpkin_gateway::{Arrival, Listener, Session};
use pumpkin_provider::Provider;
use pumpkin_schema::ir::{Command, Image};
use pumpkin_schema::provider::{
    Create, Created, Dial, Listed, Local, Published, Role, RunTag, SOCKET_NAME, SandboxId,
};
use pumpkin_schema::sandbox::tree::Limits;
use pumpkin_schema::sandbox::{
    Launch, OpPayloadLimit, PUMPKIN_BYTES, RelativePath, command_env, command_stderr,
    command_stdout, pumpkin_dir,
};
use pumpkin_schema::workflow::{Sandbox, Workflow};
use stdx::{Context, Deadline};

/// The most entries a collected tree may hold.
const ENTRIES: u64 = 100_000;

/// What a sandbox's `/pumpkin` holds at most: anything collected from below it fits.
pub(super) const PUMPKIN: Limits = Limits {
    bytes: PUMPKIN_BYTES,
    entries: ENTRIES,
};

/// The sandboxes of one run on its provider: what they run, published, and the directory of
/// their socket directories, which only the gateway's user may enter. Dropping it removes that
/// directory.
pub(super) struct Sandboxes<'p> {
    provider: &'p dyn Provider,
    run: RunTag,
    sandboxd: Published,
    /// Each role's image, as published.
    images: BTreeMap<Image, Published>,
    sockets: PathBuf,
}

/// A sandbox that started, with its session, set up.
pub(super) struct Started {
    id: SandboxId,
    pub(super) session: Session,
}

impl<'p> Sandboxes<'p> {
    /// Publishes on `provider` what `workflow`'s sandboxes run: `sandboxd`, a static binary,
    /// and every role's image; then makes the directory of the socket directories of run
    /// `run`'s sandboxes, `pumpkin-<run>` in the system's temporary directory.
    pub(super) fn create(
        provider: &'p dyn Provider,
        run: RunTag,
        workflow: &Workflow,
        sandboxd: &Path,
    ) -> Result<Self, String> {
        let judges = workflow.judges().map(|(_, judge)| &judge.sandbox);
        let roles = iter::once(&workflow.agent().sandbox).chain(judges);
        let images: BTreeSet<&Image> = roles.map(|sandbox| &sandbox.image).collect();
        let sandboxd = Local::Sandboxd {
            path: sandboxd.to_owned(),
        };
        let roles = images.iter().map(|&image| Local::Role {
            image: image.clone(),
        });
        let locals: Vec<Local> = iter::once(sandboxd).chain(roles).collect();
        let published = (provider.publish(&locals))
            .context("publishing what the run's sandboxes run failed")?;
        let all = published.len() == locals.len();
        let Some((sandboxd, roles)) = published.split_first().filter(|_| all) else {
            return Err(format!(
                "the provider published {} of the {} things the run's sandboxes run: expected a \
                 name for each",
                published.len(),
                locals.len()
            ));
        };
        let images = images.into_iter().cloned().zip(roles.iter().cloned());
        // In the system's temporary directory, since socket paths are short.
        let sockets = std::env::temp_dir().join(format!("pumpkin-{}", run.as_str()));
        DirBuilder::new()
            .mode(0o700)
            .create(&sockets)
            .context(sockets.display())?;
        Ok(Self {
            provider,
            run,
            sandboxd: sandboxd.clone(),
            images: images.collect(),
            sockets,
        })
    }

    /// Starts a sandbox for `sandbox`'s role, its socket directory named `name` within the run,
    /// and accepts its session, set up, by `by`, its arrivals sent on `arrivals` as `wrap` makes
    /// them ([`Listener::accept`]). The sandbox makes ops whose payloads hold at most `ops`, or
    /// is passive for `None`. A sandbox that doesn't start by then is deleted, with its console
    /// quoted in the error, which says if deleting it failed.
    pub(super) fn start<T: Send + 'static>(
        &self,
        name: &str,
        sandbox: &Sandbox,
        ops: Option<OpPayloadLimit>,
        by: Deadline,
        arrivals: Sender<T>,
        wrap: fn(Arrival) -> T,
    ) -> Result<Started, String> {
        let dir = self.sockets.join(name);
        fs::create_dir(&dir).context(dir.display())?;
        let socket = dir.join(SOCKET_NAME);
        let listener = Listener::bind(socket.clone()).map_err(|error| {
            format!(
                "the session's socket can't be made at {}: {error}: expected a directory that \
                 exists, which this user can write to, by a path short enough for a Unix \
                 socket, and nothing at `{SOCKET_NAME}`, where a process that was killed \
                 leaves its socket",
                socket.display()
            )
        })?;
        #[expect(
            clippy::unreachable,
            reason = "`create` published every role's image, and a run starts sandboxes only for \
                      its workflow's roles; no type excludes it, since the published names come \
                      from the provider once the workflow is parsed, and a copy of the workflow's \
                      roles that held them would serve this one lookup"
        )]
        let Some(image) = self.images.get(&sandbox.image) else {
            unreachable!("role `{}`'s image wasn't published", sandbox.name)
        };
        let create = Create {
            run: self.run.clone(),
            role: Role {
                name: sandbox.name.clone(),
                user: sandbox.user,
                resources: sandbox.resources,
            },
            image: image.clone(),
            sandboxd: self.sandboxd.clone(),
            ops: ops.is_some(),
            dial: Dial::Socket { dir },
        };
        let Created { id } = self.provider.create(&create)?;
        let session = listener
            .accept(by, ops, false, arrivals, wrap)
            .map_err(|error| error.to_string())
            .and_then(|mut session| session.set_up(by).map(|()| session));
        match session {
            Ok(session) => Ok(Started { id, session }),
            Err(error) => Err(self.unstarted(&id, &error)),
        }
    }

    /// Why sandbox `id`, whose session wasn't set up for `error`, didn't start: `error`, the
    /// end of its console, and whether deleting it failed, which this tries.
    fn unstarted(&self, id: &SandboxId, error: &str) -> String {
        let mut parts = vec![format!("sandbox {}: {error}", id.as_str())];
        match self.provider.console(&self.run, id) {
            Ok(console) if console.text.trim().is_empty() => {}
            Ok(console) => parts.push(format!("its console ends: {}", console.text.trim())),
            Err(error) => parts.push(format!("its console can't be read: {error}")),
        }
        let deleted = self.provider.delete(&self.run, std::slice::from_ref(id));
        if let Err(error) = deleted {
            parts.push(format!("deleting it failed: {error}"));
        }
        parts.join("; ")
    }

    /// Deletes a sandbox that started, then ends its session.
    pub(super) fn delete(&self, started: Started) -> Result<(), String> {
        let Started { id, session } = started;
        let deleted = self.provider.delete(&self.run, std::slice::from_ref(&id));
        drop(session);
        deleted
            .map(|_| ())
            .context(format_args!("deleting sandbox {} failed", id.as_str()))
    }

    /// Deletes every sandbox of the run that its provider still lists, and returns their ids.
    pub(super) fn sweep(&self) -> Result<Vec<SandboxId>, String> {
        let listed =
            (self.provider.list(&self.run)).context("listing the run's sandboxes failed")?;
        let ids: Vec<SandboxId> = listed.into_iter().map(|Listed { id }| id).collect();
        if ids.is_empty() {
            return Ok(ids);
        }
        let unknown = (self.provider.delete(&self.run, &ids))
            .context("deleting the sandboxes the run left failed")?;
        Ok(ids.into_iter().filter(|id| !unknown.contains(id)).collect())
    }
}

impl Drop for Sandboxes<'_> {
    fn drop(&mut self) {
        // Each listener has removed its socket by now, so this removes empty directories, which
        // hold nothing a run needs: a failure to remove them is left unreported.
        let _ = fs::remove_dir_all(&self.sockets);
    }
}

/// How to start `command` in a sandbox: as the IR says, in [`command_env()`]'s environment, with
/// an empty standard input, and its output at [`command_stdout()`] and [`command_stderr()`].
pub(super) fn launch(command: &Command) -> Launch {
    let Command { argv, env, cwd } = command;
    Launch {
        argv: argv.clone(),
        env: command_env(env),
        cwd: cwd.clone(),
        stdin: None,
        stdout: pumpkin_dir().join(&command_stdout()),
        stderr: pumpkin_dir().join(&command_stderr()),
    }
}

/// Collects the sandbox's `dir`, below [`pumpkin_dir()`], into the same path below `record`, a
/// new directory there, within `limits`, by `by`. After a failure, it holds what arrived before
/// it.
pub(super) fn collect(
    session: &mut Session,
    dir: &RelativePath,
    record: &Path,
    limits: Limits,
    by: Deadline,
) -> Result<(), String> {
    let from = pumpkin_dir().join(dir);
    session
        .collect(&from, &record.join(dir.as_str()), limits, by)
        .context(format_args!("collecting {from} failed"))
}
