//! A simulated provider, [`Sim`], which stands in for a platform: each sandbox is a thread of
//! this process, running a fake `sandboxd` that dials its create's [`Dial`] at once and answers
//! the session's requests, running each command by its program ([`Sim`]).

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::collections::BTreeMap;
use std::fs;
use std::net::Shutdown;
use std::num::NonZeroU64;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use pumpkin_provider::Provider;
use pumpkin_schema::ir::Image;
use pumpkin_schema::provider::{
    Capabilities, Console, Create, Created, Dial, Listed, Local, Published, Ready, RunTag,
    SOCKET_NAME, SandboxId,
};
use pumpkin_schema::sandbox::{Entry, ExitStatus, FromSandbox, RelativePath, ToSandbox, frame};
use serde_json::json;

use crate::provider::{Locals, Subject};

/// The path of a `sandboxd` that never dials: a create that names it, as published, makes a
/// sandbox whose `sandboxd` prints so, and ends.
pub(crate) const MUTE: &str = "/sim/mute";

/// The path of a `sandboxd` whose create fails, leaving behind a sandbox, as [`MUTE`]'s, that
/// the run's list still finds.
pub(crate) const LOST: &str = "/sim/lost";

/// What every tree the simulated `sandboxd` collects holds: a file of this name, which holds the
/// same.
const COLLECTED: &str = "collected";

/// The path of a `sandboxd` that the simulated provider refuses to publish.
pub(crate) const UNPUBLISHED: &str = "/sim/unpublished";

/// The path of a `sandboxd` that dials, but whose sandbox the simulated provider refuses to
/// delete.
pub(crate) const UNDELETABLE: &str = "/sim/undeletable";

/// A provider whose sandboxes are threads, each running a fake `sandboxd`: it dials, unless its
/// create names [`MUTE`] or [`LOST`], and then answers each request, and stops reading once
/// [stalled](Subject::stall). It puts nothing, and collects a tree of one file, [`COLLECTED`],
/// from any directory. A command whose program is `exit` exits at once with the status its
/// argument gives; one whose program is `stall` starts, then stalls the sandbox; one whose
/// program is `crash` starts, then its `sandboxd` ends the session; any other runs until the
/// sandbox is frozen. It refuses to publish [`UNPUBLISHED`], and to delete a sandbox whose
/// create names [`UNDELETABLE`].
pub(crate) struct Sim {
    ready: Ready,
    // `Provider`'s methods take `&self`, and it is `Sync`, so what they change is behind a lock.
    state: Mutex<State>,
}

struct State {
    /// The number in the next sandbox's id.
    next: u64,
    /// The sandboxes made and not yet deleted, by id.
    sandboxes: BTreeMap<String, Sandbox>,
    /// The permission bits of the directory that held each create's socket directory, in the
    /// order of the creates.
    modes: Vec<u32>,
}

struct Sandbox {
    run: RunTag,
    undeletable: bool,
    // Shared with the sandbox's thread, which prints to its console and hands over its
    // connection.
    shared: Arc<Mutex<Status>>,
    thread: JoinHandle<()>,
}

impl Sandbox {
    fn of(&self, run: &RunTag) -> bool {
        self.run.as_str() == run.as_str()
    }
}

/// What a sandbox's thread and the [`Sim`] share.
#[derive(Default)]
struct Status {
    console: String,
    /// The thread's connection once dialed, by which a delete ends the session.
    connection: Option<UnixStream>,
    stalled: bool,
    deleted: bool,
}

impl Sim {
    pub(crate) fn new() -> Self {
        // Far longer than a thread takes to start and dial, so that a busy machine makes a start
        // wait longer, never fail.
        Self::with_startup(30)
    }

    /// A simulated provider that declares `seconds` as its startup.
    pub(crate) fn with_startup(seconds: u64) -> Self {
        Self {
            ready: Ready {
                report: json!({ "provider": "sim" }),
                capabilities: Capabilities {
                    startup_seconds: NonZeroU64::new(seconds).unwrap(),
                },
            },
            state: Mutex::new(State {
                next: 1,
                sandboxes: BTreeMap::new(),
                modes: Vec::new(),
            }),
        }
    }

    /// The permission bits of the directory that held each create's socket directory, in the
    /// order of the creates.
    pub(crate) fn modes(&self) -> Vec<u32> {
        self.state().modes.clone()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        stdx::unpoisoned(self.state.lock())
    }
}

impl Provider for Sim {
    fn ready(&self) -> &Ready {
        &self.ready
    }

    fn publish(&self, locals: &[Local]) -> Result<Vec<Published>, String> {
        let published = locals.iter().map(|local| match local {
            Local::Role { image } => Ok(Published::from(image.to_string())),
            Local::Sandboxd { path } if path.as_os_str() == UNPUBLISHED => Err(format!(
                "{UNPUBLISHED} can't be published by the simulated provider: name another path"
            )),
            Local::Sandboxd { path } => Ok(Published::from(path.display().to_string())),
        });
        published.collect()
    }

    fn create(&self, create: &Create) -> Result<Created, String> {
        let Dial::Socket { dir } = &create.dial;
        let (socket, lost) = (dir.join(SOCKET_NAME), create.sandboxd.as_str() == LOST);
        let mute = lost || create.sandboxd.as_str() == MUTE;
        let undeletable = create.sandboxd.as_str() == UNDELETABLE;
        let parent = fs::metadata(dir.parent().unwrap()).unwrap();
        let mode = parent.permissions().mode() & 0o777;
        let shared = Arc::new(Mutex::default());
        let ours = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("sim-sandboxd".to_owned())
            .spawn(move || sandboxd(&socket, mute, &ours))
            .map_err(|error| format!("the sandbox's thread didn't start: {error}"))?;
        let mut state = self.state();
        let id = format!("sim-{}", state.next);
        state.next += 1;
        state.modes.push(mode);
        let run = create.run.clone();
        let sandbox = Sandbox {
            run,
            undeletable,
            shared,
            thread,
        };
        state.sandboxes.insert(id.clone(), sandbox);
        if lost {
            return Err(
                "the simulated create failed, and can't name what it made: expected the run's \
                 list to find it"
                    .to_owned(),
            );
        }
        Ok(Created {
            id: SandboxId::from(id),
        })
    }

    fn delete(&self, run: &RunTag, ids: &[SandboxId]) -> Result<Vec<SandboxId>, String> {
        let (mut deleted, mut kept, mut unknown) = (Vec::new(), Vec::new(), Vec::new());
        let mut state = self.state();
        for id in ids {
            let ours = (state.sandboxes.get(id.as_str())).filter(|sandbox| sandbox.of(run));
            match ours.map(|sandbox| sandbox.undeletable) {
                Some(true) => kept.push(id.as_str()),
                Some(false) => deleted.extend(state.sandboxes.remove(id.as_str())),
                None => unknown.push(id.clone()),
            }
        }
        // Each thread ends once told, and joining it waits for nothing the lock holds.
        drop(state);
        deleted.into_iter().for_each(stop);
        if !kept.is_empty() {
            return Err(format!(
                "{} can't be deleted by the simulated provider: create one with another `sandboxd`",
                kept.join(", ")
            ));
        }
        Ok(unknown)
    }

    fn list(&self, run: &RunTag) -> Result<Vec<Listed>, String> {
        let state = self.state();
        let ours = (state.sandboxes.iter()).filter(|(_, sandbox)| sandbox.of(run));
        let listed = ours.map(|(id, _)| Listed {
            id: SandboxId::from(id.clone()),
        });
        Ok(listed.collect())
    }

    fn console(&self, run: &RunTag, id: &SandboxId) -> Result<Console, String> {
        let state = self.state();
        let ours = state.sandboxes.get(id.as_str());
        let Some(sandbox) = ours.filter(|sandbox| sandbox.of(run)) else {
            return Err(format!(
                "{} is no sandbox of run {}: name one that the run's list finds",
                id.as_str(),
                run.as_str()
            ));
        };
        let text = status(&sandbox.shared).console.clone();
        Ok(Console { text })
    }
}

impl Subject for Sim {
    fn locals(&self, _: &Path) -> Locals {
        Locals {
            image: Image::try_from(format!("sim@sha256:{}", "0".repeat(64))).unwrap(),
            sandboxd: PathBuf::from("/sim/sandboxd"),
            mute: PathBuf::from(MUTE),
        }
    }

    fn stall(&self, id: &SandboxId) {
        let state = self.state();
        status(&state.sandboxes[id.as_str()].shared).stalled = true;
    }
}

fn status(shared: &Mutex<Status>) -> MutexGuard<'_, Status> {
    stdx::unpoisoned(shared.lock())
}

/// Tells a deleted sandbox's thread to end, closing its connection if it has one, and waits
/// until it has.
fn stop(sandbox: Sandbox) {
    let Sandbox {
        run: _,
        undeletable: _,
        shared,
        thread,
    } = sandbox;
    let mut status = status(&shared);
    status.deleted = true;
    if let Some(connection) = status.connection.take() {
        // A connection the gateway closed already is fine.
        let _ = connection.shutdown(Shutdown::Both);
    }
    drop(status);
    // Only a bug panics the thread, and joining raises its panic here.
    thread.join().unwrap();
}

/// A sandbox's fake `sandboxd`: it prints why it doesn't dial if `mute`, or else dials `socket`
/// and serves the session until the session ends, the sandbox is deleted, or it is stalled.
fn sandboxd(socket: &Path, mute: bool, shared: &Mutex<Status>) {
    if mute {
        (status(shared).console).push_str("sandboxd: a simulated one, which never dials\n");
        return;
    }
    let dialed = UnixStream::connect(socket).and_then(|connection| {
        let other = connection.try_clone()?;
        Ok((connection, other))
    });
    let mut status = status(shared);
    match dialed {
        // A sandbox deleted as it dialed has no session to serve.
        Ok(_) if status.deleted => {}
        Ok((connection, other)) => {
            status.connection = Some(other);
            drop(status);
            serve(connection, shared);
        }
        Err(error) => {
            let socket = socket.display();
            let failed = format!("sandboxd: connecting to the gateway at {socket} failed: {error}");
            status.console.push_str(&failed);
        }
    }
}

/// Answers the session's requests on `connection` until the session ends, or, once stalled,
/// reads nothing more.
fn serve(mut connection: UnixStream, shared: &Mutex<Status>) {
    let (mut replies, mut crashed) = (Vec::<(FromSandbox, &[u8])>::new(), false);
    // A connection that ends, cleanly or not, ends the session, as a delete's does.
    while let Ok(Some((request, _))) = frame::read::<ToSandbox>(&mut connection) {
        if status(shared).stalled {
            // It reads nothing more, as a stuck `sandboxd`, and answers nothing; the connection
            // stays open, by the clone its status keeps until a delete shuts it down, so the
            // session's writes fill the connection's buffers.
            return;
        }
        match request {
            ToSandbox::Put { .. } | ToSandbox::Entry { .. } | ToSandbox::Data { .. } => {}
            ToSandbox::End { stream } | ToSandbox::Freeze { stream } => {
                replies.push((FromSandbox::Done { stream }, &[]));
            }
            ToSandbox::Start { stream, launch } => {
                replies.push((FromSandbox::Done { stream }, &[]));
                match launch.argv.program() {
                    "exit" => {
                        let code = launch.argv.args()[0].parse().unwrap();
                        let status = ExitStatus::Code(code);
                        replies.push((FromSandbox::Exited { stream, status }, &[]));
                    }
                    "stall" => status(shared).stalled = true,
                    "crash" => crashed = true,
                    _ => {}
                }
            }
            ToSandbox::Collect { stream, .. } => {
                let path = RelativePath::try_from(COLLECTED.to_owned()).unwrap();
                let size = u64::try_from(COLLECTED.len()).unwrap();
                let entry = Entry::File { path, size };
                replies.push((FromSandbox::Entry { stream, entry }, &[]));
                replies.push((FromSandbox::Data { stream }, COLLECTED.as_bytes()));
                replies.push((FromSandbox::End { stream }, &[]));
            }
            // It makes no ops, so a reply to one breaks the protocol, which ends the session.
            ToSandbox::OpReply { .. } => return,
        }
        for (reply, body) in replies.drain(..) {
            if frame::write(&mut connection, &reply, body).is_err() {
                return;
            }
        }
        if crashed {
            // A shutdown closes the connection for the clone its status keeps too.
            let _ = connection.shutdown(Shutdown::Both);
            return;
        }
    }
}
