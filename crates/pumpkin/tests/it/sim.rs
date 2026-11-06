//! A simulated provider, [`Sim`], which stands in for a platform: each sandbox is a thread of
//! this process, running a fake `sandboxd` that dials its create's [`Dial`] at once and answers
//! the session's requests, running each command by its program, which may make ops or judge a
//! payload ([`Sim`]).

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::net::Shutdown;
use std::num::NonZeroU64;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use pumpkin_provider::Provider;
use pumpkin_schema::ir::{AbsolutePath, Image, Name};
use pumpkin_schema::provider::{
    Capabilities, Console, Create, Created, Dial, Listed, Local, Published, Ready, RunTag,
    SOCKET_NAME, SandboxId,
};
use pumpkin_schema::sandbox::op::{JobState, OpCall, OpReply, StatusReply};
use pumpkin_schema::sandbox::{
    Entry, ExitStatus, FromSandbox, RESULT_MAX_BYTES, RelativePath, ToSandbox, frame, out_dir,
    pumpkin_dir,
};
use serde_json::json;

use crate::provider::{Locals, Subject};

/// The path of a `sandboxd`, or an image's repository, that never dials: a create that names
/// it, as published, makes a sandbox whose `sandboxd` prints so, and ends.
pub(crate) const MUTE: &str = "/sim/mute";

/// The path of a `sandboxd` whose create fails, leaving behind a sandbox, as [`MUTE`]'s, that
/// the run's list still finds.
pub(crate) const LOST: &str = "/sim/lost";

/// An image's repository whose create returns [`SLOW_CREATE`] after it starts the sandbox,
/// whose `sandboxd` has dialed by then.
pub(crate) const SLOW: &str = "/sim/slow";

/// How long a create of [`SLOW`] takes: far longer than a run takes to act on what its agent
/// does as the create begins, or a `sandboxd` takes to dial, and far shorter than the startup of
/// the providers whose tests use it.
const SLOW_CREATE: Duration = Duration::from_secs(2);

/// How long a command that awaits a job's state waits between the `status` ops it makes.
const AWAIT_POLL: Duration = Duration::from_millis(10);

/// What every tree the simulated `sandboxd` collects holds: a file of this name, which holds the
/// same.
const COLLECTED: &str = "collected";

/// The path of a `sandboxd` that the simulated provider refuses to publish.
pub(crate) const UNPUBLISHED: &str = "/sim/unpublished";

/// The path of a `sandboxd`, or an image's repository, that dials, but whose sandbox the
/// simulated provider refuses to delete.
pub(crate) const UNDELETABLE: &str = "/sim/undeletable";

/// The path of a `sandboxd`, or an image's repository, whose sandbox's delete waits [`LINGER`]
/// before it deletes it.
pub(crate) const LINGERING: &str = "/sim/lingering";

/// How long a delete of a [`LINGERING`] sandbox takes: seconds longer than the hard end of the
/// runs whose tests use it.
const LINGER: Duration = Duration::from_secs(10);

/// A provider whose sandboxes are threads, each running a fake `sandboxd`: it dials, unless its
/// create names [`MUTE`] or [`LOST`], and then answers each request, and stops reading once
/// [stalled](Subject::stall). It keeps the files put, and collects a tree of one file,
/// [`COLLECTED`], from any directory, and from `out` a judge's result too. A command whose
/// program is `exit` exits at once with the status its argument gives; `stall` starts, then
/// stalls the sandbox; `crash` starts, then its `sandboxd` ends the session; `ops` takes the
/// steps its arguments give ([`step`]), making each op once the one before it is answered, then
/// exits with 0, unless it has, or its `sandboxd` ends the session as a step asks; `judge`
/// judges the payload put at the path its argument gives ([`judge`]); any other runs until the
/// sandbox is frozen. It refuses to publish [`UNPUBLISHED`], and to delete a sandbox whose
/// create names [`UNDELETABLE`]; it takes [`SLOW_CREATE`] to create one that names [`SLOW`],
/// and [`LINGER`] to delete one that names [`LINGERING`].
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
    lingering: bool,
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
        let image = create.image.as_str();
        let repository = image
            .split_once('@')
            .map_or(image, |(repository, _)| repository);
        let names = |fault: &str| create.sandboxd.as_str() == fault || repository == fault;
        let (socket, lost) = (dir.join(SOCKET_NAME), names(LOST));
        let (mute, undeletable, lingering) =
            (lost || names(MUTE), names(UNDELETABLE), names(LINGERING));
        let parent = fs::metadata(dir.parent().unwrap()).unwrap();
        let mode = parent.permissions().mode() & 0o777;
        let shared = Arc::new(Mutex::default());
        let ours = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("sim-sandboxd".to_owned())
            .spawn(move || sandboxd(&socket, mute, &ours))
            .map_err(|error| format!("the sandbox's thread didn't start: {error}"))?;
        if names(SLOW) {
            thread::sleep(SLOW_CREATE);
        }
        let mut state = self.state();
        let id = format!("sim-{}", state.next);
        state.next += 1;
        state.modes.push(mode);
        let run = create.run.clone();
        let sandbox = Sandbox {
            run,
            undeletable,
            lingering,
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

/// An image of `repository`, pinned by a digest of zeros.
pub(crate) fn image(repository: &str) -> Image {
    Image::try_from(format!("{repository}@sha256:{}", "0".repeat(64))).unwrap()
}

impl Subject for Sim {
    fn locals(&self, _: &Path) -> Locals {
        Locals {
            image: image("sim"),
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
/// until it has, after [`LINGER`] if it is [`LINGERING`].
fn stop(sandbox: Sandbox) {
    let Sandbox {
        run: _,
        undeletable: _,
        lingering,
        shared,
        thread,
    } = sandbox;
    if lingering {
        thread::sleep(LINGER);
    }
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

/// A step of a command that makes ops.
enum Step {
    /// An op, with its payload, and, if bounded, the fewest seconds by which its reply must
    /// leave less time than the last `ops` reply did.
    Op {
        call: OpCall,
        payload: Vec<u8>,
        less: Option<u64>,
    },
    /// `status` of `job`, made until its reply gives `state`.
    Await { job: u64, state: String },
    /// A wait, in seconds, before the next step.
    Sleep(u64),
    /// The command's exit with 0, after which the sandbox takes the steps that follow, as a
    /// process the command left would.
    Exit,
    /// The session's end, which its `sandboxd` makes.
    Crash,
}

/// A command that makes ops.
struct Caller {
    /// The command's stream, on which it exits.
    stream: u32,
    /// The steps still to take, in order.
    steps: VecDeque<Step>,
    /// The number of the op in flight.
    op: u32,
    /// The bound on the reply to the op in flight, as [`Step::Op`] gives it.
    less: Option<u64>,
    /// The seconds the last `ops` reply left, once one has come.
    left: Option<u64>,
    /// The job whose `status` the op in flight asks, and the state it awaits, if it awaits one.
    awaiting: Option<(u64, String)>,
    /// Whether the command has exited.
    exited: bool,
}

impl Caller {
    /// Takes the reply to the op in flight, `reply`: if it is an `ops` that breaks the op's
    /// bound, or that follows no `ops` reply, the command exits with 1 at once, taking no more
    /// steps; if it gives another state than the one awaited, it asks again after
    /// [`AWAIT_POLL`]; else it goes on ([`Caller::next`]).
    fn answered(&mut self, reply: &OpReply, replies: &mut Vec<(FromSandbox, Vec<u8>)>) -> bool {
        if let OpReply::Ops(ops) = reply {
            let left = ops.remaining_seconds;
            if let Some(less) = self.less
                && self
                    .left
                    .is_none_or(|last| left.saturating_add(less) > last)
            {
                self.steps.clear();
                self.exit(1, replies);
            }
            self.left = Some(left);
        }
        if let Some((job, state)) = self.awaiting.take()
            && state_name(reply) != state
        {
            thread::sleep(AWAIT_POLL);
            self.make(OpCall::Status { job }, Vec::new(), None, replies);
            self.awaiting = Some((job, state));
            return false;
        }
        self.next(replies)
    }

    /// Takes the steps up to the next op, and makes it, or exits with 0 once there are none,
    /// unless it has exited, in `replies`. Returns whether the session is to end: once the op
    /// before an end is sent, before its answer.
    fn next(&mut self, replies: &mut Vec<(FromSandbox, Vec<u8>)>) -> bool {
        loop {
            let (call, payload, less) = match self.steps.pop_front() {
                Some(Step::Op {
                    call,
                    payload,
                    less,
                }) => (call, payload, less),
                Some(Step::Await { job, state }) => {
                    self.awaiting = Some((job, state));
                    (OpCall::Status { job }, Vec::new(), None)
                }
                Some(Step::Sleep(seconds)) => {
                    thread::sleep(Duration::from_secs(seconds));
                    continue;
                }
                Some(Step::Exit) => {
                    self.exit(0, replies);
                    continue;
                }
                Some(Step::Crash) => return true,
                None => {
                    self.exit(0, replies);
                    return false;
                }
            };
            self.make(call, payload, less, replies);
            return matches!(self.steps.front(), Some(Step::Crash));
        }
    }

    /// Makes an op of `call`, with `payload`, whose reply is bounded by `less`, as [`Step::Op`]
    /// gives it, in `replies`.
    fn make(
        &mut self,
        call: OpCall,
        payload: Vec<u8>,
        less: Option<u64>,
        replies: &mut Vec<(FromSandbox, Vec<u8>)>,
    ) {
        (self.op, self.less) = (self.op + 1, less);
        let (op, size) = (self.op, u64::try_from(payload.len()).unwrap());
        replies.push((FromSandbox::Op { op, call, size }, Vec::new()));
        if size > 0 {
            replies.push((FromSandbox::OpData { op }, payload));
        }
    }

    /// Exits with `code`, in `replies`, unless it has exited.
    fn exit(&mut self, code: i32, replies: &mut Vec<(FromSandbox, Vec<u8>)>) {
        if self.exited {
            return;
        }
        let (stream, status) = (self.stream, ExitStatus::Code(code));
        replies.push((FromSandbox::Exited { stream, status }, Vec::new()));
        self.exited = true;
    }
}

/// The state a status reply gives, as `sandboxctl` prints it, or `""` for any other reply.
fn state_name(reply: &OpReply) -> &'static str {
    let OpReply::Status(StatusReply { state, .. }) = reply else {
        return "";
    };
    match state {
        JobState::Queued => "queued",
        JobState::Running => "running",
        JobState::Passed { .. } => "passed",
        JobState::Failed(_) => "failed",
    }
}

/// The step that `arg` gives: an op, `ops`, which `ops at least <seconds> less` bounds, `status
/// <job>`, `judge <grant> <payload>`, `submit <grant> <payload>` or `finalize <grant>
/// <payload>`, a final submission, where a payload that is a number is that many bytes of `x`,
/// and any other is its own bytes; `await <job> <state>`, which makes `status <job>` until its
/// reply's state is `state`; or `sleep <seconds>`, `exit` or `crash`.
fn step(arg: &str) -> Step {
    let words: Vec<&str> = arg.split(' ').collect();
    let grant = || Name::try_from(words[1].to_owned()).unwrap();
    let payload = || match words[2].parse() {
        Ok(size) => vec![b'x'; size],
        Err(_) => words[2].as_bytes().to_vec(),
    };
    let (call, payload, less) = match words[..] {
        ["ops"] => (OpCall::Ops, Vec::new(), None),
        ["ops", "at", "least", seconds, "less"] => {
            (OpCall::Ops, Vec::new(), Some(seconds.parse().unwrap()))
        }
        ["status", job] => {
            let job = job.parse().unwrap();
            (OpCall::Status { job }, Vec::new(), None)
        }
        ["judge", _, _] => (OpCall::Judge { grant: grant() }, payload(), None),
        [kind @ ("submit" | "finalize"), _, _] => {
            let finalize = kind == "finalize";
            let grant = grant();
            (OpCall::Submit { grant, finalize }, payload(), None)
        }
        ["await", job, state] => {
            let (job, state) = (job.parse().unwrap(), state.to_owned());
            return Step::Await { job, state };
        }
        ["sleep", seconds] => return Step::Sleep(seconds.parse().unwrap()),
        ["exit"] => return Step::Exit,
        ["crash"] => return Step::Crash,
        _ => panic!("`{arg}` is no step of the simulated sandboxd's"),
    };
    Step::Op {
        call,
        payload,
        less,
    }
}

/// The files put in a sandbox, by path.
#[derive(Default)]
struct Files {
    by_path: BTreeMap<String, Vec<u8>>,
    /// The directory of the tree being put, if one is.
    dir: Option<AbsolutePath>,
    /// The path of the file being put, if one is.
    file: Option<String>,
}

impl Files {
    fn entry(&mut self, entry: Entry) {
        let (Some(dir), Entry::File { path, .. }) = (&self.dir, entry) else {
            return;
        };
        let path = dir.join(&path).to_string();
        self.by_path.insert(path.clone(), Vec::new());
        self.file = Some(path);
    }

    fn data(&mut self, body: &[u8]) {
        let file = (self.file.as_ref()).and_then(|file| self.by_path.get_mut(file));
        file.unwrap().extend_from_slice(body);
    }
}

/// What a `judge` command, whose exit comes on `stream`, does with `payload`, the file put at
/// the path its argument gives, if there is one: for `slow`, it runs until the sandbox is
/// frozen; for `stall`, it stalls the sandbox; for any other, it writes the result
/// `{"status": "passed", "raw_score": n}`, where `n` is the payload's size, which it returns,
/// and exits with 0, the result padded with spaces to a byte over [`RESULT_MAX_BYTES`] for
/// `large`. Without a payload, it exits with 1.
fn judge(
    payload: Option<&Vec<u8>>,
    stream: u32,
    replies: &mut Vec<(FromSandbox, Vec<u8>)>,
    shared: &Mutex<Status>,
) -> Option<Vec<u8>> {
    let exit = |code| {
        let status = ExitStatus::Code(code);
        (FromSandbox::Exited { stream, status }, Vec::new())
    };
    match payload.map(Vec::as_slice) {
        Some(b"slow") => None,
        Some(b"stall") => {
            status(shared).stalled = true;
            None
        }
        Some(payload) => {
            replies.push(exit(0));
            let result = json!({ "status": "passed", "raw_score": payload.len() });
            let mut result = result.to_string().into_bytes();
            if payload == b"large" {
                result.resize(usize::try_from(RESULT_MAX_BYTES).unwrap() + 1, b' ');
            }
            Some(result)
        }
        None => {
            replies.push(exit(1));
            None
        }
    }
}

/// Collects `dir` on `stream`, in `replies`: [`COLLECTED`], and from `out` the judge's
/// `result` too, if it wrote one.
fn collect(
    stream: u32,
    dir: &AbsolutePath,
    result: Option<&[u8]>,
    replies: &mut Vec<(FromSandbox, Vec<u8>)>,
) {
    let mut tree = vec![(COLLECTED, COLLECTED.as_bytes())];
    if *dir == pumpkin_dir().join(&out_dir()) {
        tree.extend(result.map(|result| ("result.json", result)));
    }
    for (path, body) in tree {
        let path = RelativePath::try_from(path.to_owned()).unwrap();
        let size = u64::try_from(body.len()).unwrap();
        let entry = Entry::File { path, size };
        replies.push((FromSandbox::Entry { stream, entry }, Vec::new()));
        for data in body.chunks(frame::MAX_BODY_BYTES) {
            replies.push((FromSandbox::Data { stream }, data.to_vec()));
        }
    }
    replies.push((FromSandbox::End { stream }, Vec::new()));
}

/// Answers the session's requests on `connection` until the session ends, or, once stalled,
/// reads nothing more.
fn serve(mut connection: UnixStream, shared: &Mutex<Status>) {
    let (mut replies, mut crashed) = (Vec::<(FromSandbox, Vec<u8>)>::new(), false);
    let (mut caller, mut files, mut result) = (None::<Caller>, Files::default(), None);
    // A connection that ends, cleanly or not, ends the session, as a delete's does.
    while let Ok(Some((request, body))) = frame::read::<ToSandbox>(&mut connection) {
        if status(shared).stalled {
            // It reads nothing more, as a stuck `sandboxd`, and answers nothing; the connection
            // stays open, by the clone its status keeps until a delete shuts it down, so the
            // session's writes fill the connection's buffers.
            return;
        }
        match request {
            ToSandbox::Put { dir, .. } => files.dir = Some(dir),
            ToSandbox::Entry { entry, .. } => files.entry(entry),
            ToSandbox::Data { .. } => files.data(&body),
            ToSandbox::End { stream } | ToSandbox::Freeze { stream } => {
                replies.push((FromSandbox::Done { stream }, Vec::new()));
            }
            ToSandbox::Start { stream, launch } => {
                replies.push((FromSandbox::Done { stream }, Vec::new()));
                match launch.argv.program() {
                    "exit" => {
                        let code = launch.argv.args()[0].parse().unwrap();
                        let status = ExitStatus::Code(code);
                        replies.push((FromSandbox::Exited { stream, status }, Vec::new()));
                    }
                    "stall" => status(shared).stalled = true,
                    "crash" => crashed = true,
                    "ops" => {
                        let steps = launch.argv.args().iter().map(|arg| step(arg)).collect();
                        let made = caller.insert(Caller {
                            stream,
                            steps,
                            op: 0,
                            less: None,
                            left: None,
                            awaiting: None,
                            exited: false,
                        });
                        crashed = made.next(&mut replies);
                    }
                    "judge" => {
                        let payload = files.by_path.get(&launch.argv.args()[0]);
                        result = judge(payload, stream, &mut replies, shared);
                    }
                    _ => {}
                }
            }
            ToSandbox::Collect { stream, dir } => {
                collect(stream, &dir, result.as_deref(), &mut replies);
            }
            ToSandbox::OpReply { op, reply } => match &mut caller {
                Some(caller) if caller.op == op => crashed = caller.answered(&reply, &mut replies),
                // A reply to no op in flight breaks the protocol, which ends the session.
                Some(_) | None => return,
            },
        }
        for (reply, body) in replies.drain(..) {
            if frame::write(&mut connection, &reply, &body).is_err() {
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
