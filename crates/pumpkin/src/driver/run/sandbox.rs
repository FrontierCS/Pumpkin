//! What the agent's sandbox and the judges' share: starting a sandbox for a role, launching
//! its IR command, and collecting what it wrote.

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use pumpkin_docker::{Container, Engine, RunTag, Start};
use pumpkin_gateway::Session;
use pumpkin_schema::ir::Command;
use pumpkin_schema::sandbox::tree::Limits;
use pumpkin_schema::sandbox::{
    Launch, OpPayloadLimit, PUMPKIN_BYTES, RelativePath, command_env, command_stderr,
    command_stdout, pumpkin_dir,
};
use pumpkin_schema::workflow::Sandbox;
use stdx::Context;

use super::record;

/// The most entries a collected tree may hold.
const ENTRIES: u64 = 100_000;

/// What a sandbox's `/pumpkin` holds at most: anything collected from below it fits.
pub(super) const PUMPKIN: Limits = Limits {
    bytes: PUMPKIN_BYTES,
    entries: ENTRIES,
};

/// Starts the sandboxes of one run, through the Docker provider.
#[derive(Debug)]
pub(super) struct Sandboxes {
    tag: RunTag,
    engine: Engine,
    sandboxd: PathBuf,
    /// The directory of the sandboxes' socket directories, which only the gateway's user may
    /// enter. A sandbox sees its own socket directory alone, mounted.
    sockets: PathBuf,
}

impl Sandboxes {
    /// Sandboxes for run `run`, tagged with its id, on `engine`, with their socket directories
    /// in a new directory, [`sockets`].
    pub(super) fn create(run: &str, engine: Engine, sandboxd: &Path) -> Result<Self, String> {
        let sandboxd = sandboxd.canonicalize().map_err(|error| {
            format!(
                "`sandboxd` at {} can't be resolved: {error}: give the path of the static \
                 binary that `cargo xtask sandboxd` builds",
                sandboxd.display()
            )
        })?;
        let sockets = sockets(run);
        DirBuilder::new()
            .mode(0o700)
            .create(&sockets)
            .context(sockets.display())?;
        Ok(Self {
            tag: RunTag::from(run.to_owned()),
            engine,
            sandboxd,
            sockets,
        })
    }

    /// Starts a sandbox for `sandbox`'s role, its socket directory named `name` within the run.
    /// It makes ops if `ops` gives their payload limit, which the provider passes when accepting
    /// the session ([`Start::ops`]); `None` starts a passive one.
    pub(super) fn start(
        &self,
        name: &str,
        sandbox: &Sandbox,
        ops: Option<OpPayloadLimit>,
    ) -> Result<Container, String> {
        let socket_dir = self.sockets.join(name);
        fs::create_dir(&socket_dir).context(socket_dir.display())?;
        let start = Start {
            run: &self.tag,
            sandbox,
            sandboxd: &self.sandboxd,
            socket_dir: &socket_dir,
            ops,
        };
        self.engine.start(&start)
    }

    /// Removes the socket directories, and every sandbox of the run that is left, and returns
    /// the sandboxes that removed. Fails, saying which, if either can't be removed.
    pub(super) fn sweep(&self) -> Result<Vec<String>, String> {
        let sockets = match fs::remove_dir_all(&self.sockets) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(format!(
                "removing the run's socket directories failed: {}: {error}",
                self.sockets.display()
            )),
            Ok(()) | Err(_) => Ok(()),
        };
        let removed =
            (self.engine.destroy_all(&self.tag)).context("removing the run's containers failed");
        match (removed, sockets) {
            (removed, Ok(())) => removed,
            (Ok(_), Err(error)) => Err(error),
            (Err(removed), Err(error)) => Err(format!("{removed}; {error}")),
        }
    }
}

impl Drop for Sandboxes {
    fn drop(&mut self) {
        // A run sweeps itself; a panic that unwinds, as in tests, skips that, so the sweep
        // happens here instead. A panic that aborts the process, as `stdx::abort_on_panic`
        // has it, does so before any drop.
        if std::thread::panicking() {
            let _ = self.sweep();
        }
    }
}

/// The directory of run `run`'s socket directories: one in the system's temporary
/// directory, since socket paths are short.
fn sockets(run: &str) -> PathBuf {
    std::env::temp_dir().join(format!("pumpkin-{run}"))
}

/// How to start `command` in a sandbox: as the IR says, in [`command_env()`]'s environment, with
/// its output at [`command_stdout()`] and [`command_stderr()`].
pub(super) fn launch(command: &Command) -> Launch {
    let Command { argv, env, cwd } = command;
    Launch {
        argv: argv.clone(),
        env: command_env(env),
        cwd: cwd.clone(),
        stdout: pumpkin_dir().join(&command_stdout()),
        stderr: pumpkin_dir().join(&command_stderr()),
    }
}

/// Collects the sandbox's `dir`, below [`pumpkin_dir()`], into the same path below `record`, a
/// new directory there, within `limits`. After a failure, it holds what arrived before it.
pub(super) fn collect(
    session: &mut Session,
    dir: &RelativePath,
    record: &Path,
    limits: Limits,
) -> Result<(), String> {
    let from = pumpkin_dir().join(dir);
    session
        .collect(&from, &record.join(dir.as_str()), limits)
        .context(format_args!("collecting {from} failed"))
}

/// A run's clock: the time since the run started, and when that was.
#[derive(Clone, Copy, Debug)]
pub(super) struct Clock {
    start: Instant,
    /// The start, as the time since the Unix epoch.
    unix: Duration,
}

impl Clock {
    pub(super) fn start() -> Self {
        Self {
            start: Instant::now(),
            unix: record::since_epoch(SystemTime::now()),
        }
    }

    /// When the run started, as the time since the Unix epoch ([`record::since_epoch`]).
    pub(super) fn unix(self) -> Duration {
        self.unix
    }

    pub(super) fn now(self) -> Duration {
        self.start.elapsed()
    }

    /// The time of `instant`, which is after the start.
    pub(super) fn at(self, instant: Instant) -> Duration {
        instant.saturating_duration_since(self.start)
    }
}
