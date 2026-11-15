//! The gateway's end of the sessions that sandboxes open, by [the protocol](schema::sandbox).
//!
//! A sandbox meets the gateway where its [`Registration`] says, which accepts the [`Session`] the
//! sandbox opens with its hello.
//!
//! A sandbox, however buggy, can't harm the host through its session: a message that fails to
//! parse or comes out of turn, more replies than a waiting request takes, or a malformed tree
//! ends the session, which stops the sandbox. A late reply, on a stream whose request has given
//! up, is dropped. A collected tree is written into a new directory of its own, within
//! [`Limits`]: a tree that passes them fails only its collect.
//!
//! # Deadlines
//!
//! Every request waits on the sandbox until its caller's deadline, so a slow link makes it wait,
//! never fail. A request whose deadline passes fails with [`io::ErrorKind::TimedOut`] and ends
//! the session; so does a sandbox that closes its connection, at once. A frame may also wait
//! behind another writer's, until that writer's deadline.
//!
//! # Ops and model requests
//!
//! A session takes them if [`Registration::accept`] says so, and each reaches the owner as an
//! [`Arrival`]: an op is answered with [`Session::reply`] or [`Session::refuse`], and a model
//! request with [`IncomingModelRequest::respond`], from any thread. The gateway authorizes
//! nothing, but bounds what they cost the host, and ends a session that breaks their rules.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::num::NonZeroU64;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::{fmt, fs};

pub use meeting::{Dialer, GatewayAddress, GrantListener, Registration, Sockets, Unreached};
use schema::ir::{AbsolutePath, Argv, GpuModel, Mode, Name};
use schema::sandbox::op::{OpCall, OpReply};
use schema::sandbox::tree::{self, Limits, Modes};
use schema::sandbox::{
    Entry, ExitStatus, FromSandbox, Launch, MAX_MODEL_REQUEST_BYTES, MAX_MODEL_REQUESTS_IN_FLIGHT,
    MAX_OP_PAYLOAD_BYTES, OpPayloadLimit, RelativePath, Root, ToSandbox, dir_mode, frame,
    pumpkin_dir, pumpkin_subdirs,
};
use stdx::Deadline;

mod meeting;

/// The number of the next session the process makes, which tells it apart from every other, so
/// that an op is answered only on the session it arrived on.
// Shared by every session the process makes, on whatever thread makes it.
static NEXT_SESSION: AtomicU64 = AtomicU64::new(0);

/// A sandbox's session, which one owner drives, while the responses to its model requests
/// stream back beside it ([`IncomingModelRequest::respond`]), and trees move through its
/// [`Trees`].
///
/// Requests on it block until `sandboxd` answers, their deadline passes, or the session ends,
/// and their frames may wait behind other writers' ([deadlines](crate#deadlines)). Dropping it
/// ends the session, which stops the sandbox, and returns once the reader has sent its last
/// arrival, the end, which waits for a collect through its [`Trees`] to take the part the reader
/// holds.
#[derive(Debug)]
pub struct Session {
    /// The session's number, from [`NEXT_SESSION`], which its ops and commands carry.
    id: u64,
    // Shared with the reader thread, and with the responses to model requests, which stream from
    // the threads that serve them while the owner sends requests.
    shared: Arc<Shared>,
    /// The reader thread, `Some` until the drop joins it.
    reader: Option<JoinHandle<()>>,
    gpus: Option<Vec<GpuModel>>,
}

/// What a session's owner, its reader thread and the responses to its model requests share.
#[derive(Debug)]
struct Shared {
    /// The connection, which every writer writes and the reader reads, and which ends the session
    /// without waiting for a writer.
    connection: Connection,
    // Each writer holds it for a whole frame, so that frames never interleave.
    writer: Mutex<()>,
    // The reader thread routes the sandbox's frames by them, removes a route once its command has
    // exited or nothing receives from it, and puts model requests in flight; the responses to
    // model requests take theirs out.
    routes: Mutex<Routes>,
    // The reader thread may end the session, and each response to a model request reports why it
    // ended. Set under the routes' lock, which `end` explains.
    ended: OnceLock<String>,
}

/// Where frames from the sandbox go: the requests waiting for them and the commands running, by
/// stream, which streams have been opened, and whether the sandbox may open an op or a model
/// request.
#[derive(Debug)]
struct Routes {
    open: BTreeMap<u32, Route>,
    /// The stream the next request opens: requests open streams in order from 1, so every
    /// stream from 1 below it has been opened.
    next_stream: u32,
    /// The op in flight, by number: opened, and not yet answered.
    op: Option<u32>,
    /// The model requests in flight, by number: opened, and their responses not yet ended.
    models: BTreeSet<u32>,
}

/// Where the frames on a stream go. Each route takes only the replies named here: any other
/// breaks the protocol.
#[derive(Debug)]
enum Route {
    /// To a request, which the session's owner waits on, for its reply: done, or failed with
    /// `sandboxd`'s message. A second that finds the queue full breaks the protocol.
    Reply(SyncSender<Result<(), String>>),
    /// To a command's start, as `Reply`; once it is done, the command runs (`Running`).
    Start(SyncSender<Result<(), String>>),
    /// To a request writing a tree as its parts arrive, until its end or `sandboxd`'s failure.
    Tree(SyncSender<Result<Part, String>>),
    /// To the owner, as an arrival: a started command's exit, its stream's last frame.
    Running,
}

/// A [`FromSandbox`] that a stream carries, without its stream.
enum Reply {
    Done,
    Failed(String),
    Exited(ExitStatus),
    Part(Part),
}

enum Part {
    Entry(Entry),
    /// Bytes of the file being collected, the frame's body.
    Data(Vec<u8>),
    End,
}

impl Reply {
    /// The frame's type, as the protocol names it.
    fn name(&self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Failed(_) => "failed",
            Self::Exited(_) => "exited",
            Self::Part(Part::Entry(_)) => "entry",
            Self::Part(Part::Data(_)) => "data",
            Self::Part(Part::End) => "end",
        }
    }
}

#[derive(Debug)]
pub enum Put<'a> {
    Dir(RelativePath, Mode),
    File(RelativePath, Mode, &'a [u8]),
    /// A file of the host's, read as it is sent, whose size is its size as the put reaches it:
    /// one that then reads short, or fails to be read, fails the put.
    Read(RelativePath, Mode, fs::File),
}

/// A session's trees, which a thread other than its owner's moves ([`Session::trees`]).
///
/// Its requests go beside the owner's, each by its deadline as any request
/// ([deadlines](crate#deadlines)); once the session has ended, each fails.
#[derive(Debug)]
pub struct Trees {
    // Shared with the session, its reader thread and the responses to its model requests.
    shared: Arc<Shared>,
}

impl Trees {
    /// Writes `entries` below `dir`, made with its missing parents as `root` says, as
    /// [`Session::put`] writes them. Fails if `dir` exists and `root` is [`Root::New`]; whatever
    /// was written before a failure stays.
    pub fn put<'a>(
        &self,
        dir: &AbsolutePath,
        root: Root,
        entries: impl IntoIterator<Item = Put<'a>>,
        by: Deadline,
    ) -> io::Result<()> {
        self.shared.put(dir, root, entries, by)
    }

    /// Copies the tree in the sandbox's `dir` into `into`, as [`Session::collect`] does, but cut
    /// short once `stopped`, which it asks before each part of the tree, returns true, with
    /// [`io::ErrorKind::Interrupted`], the session still open. Returns the tree's entries, each
    /// with its mode as the sandbox sent it, which the host's umask may have narrowed in `into`.
    pub fn collect(
        &self,
        dir: &AbsolutePath,
        into: &Path,
        limits: Limits,
        by: Deadline,
        stopped: &mut dyn FnMut() -> bool,
    ) -> io::Result<Vec<Entry>> {
        self.shared.collect(dir, into, limits, by, stopped)
    }
}

/// An op a caller in the sandbox made, whole, and in flight until [`Session::reply`] answers
/// it, on the session that handed it out.
#[derive(Debug)]
#[must_use = "an op dropped unanswered wedges the sandbox's ops, which wait on it for good: \
              answer it with `Session::reply`"]
pub struct IncomingOp {
    session: u64,
    /// The sandbox's number for the op, which the reply names.
    number: u32,
    pub op: Op,
}

/// What an op asks of the session's owner: an [`OpCall`], with the payload that a judge call or
/// a submission takes, of at most the session's limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// See [`OpCall::Ops`].
    Ops,
    /// See [`OpCall::Judge`], whose payload is `payload`.
    Judge { grant: Name, payload: Vec<u8> },
    /// See [`OpCall::Submit`], whose payload is `payload`.
    Submit {
        grant: Name,
        finalize: bool,
        payload: Vec<u8>,
    },
    /// See [`OpCall::Status`].
    Status { job: u64 },
    /// See [`OpCall::LeaseOpen`].
    LeaseOpen {
        grant: Name,
        instance_seconds: Option<NonZeroU64>,
    },
    /// See [`OpCall::LeaseRun`].
    LeaseRun {
        lease: u64,
        argv: Argv,
        timeout_seconds: Option<NonZeroU64>,
    },
    /// See [`OpCall::LeasePut`].
    LeasePut {
        lease: u64,
        tree: AbsolutePath,
        to: AbsolutePath,
        replace: bool,
    },
    /// See [`OpCall::LeaseGet`].
    LeaseGet {
        lease: u64,
        from: AbsolutePath,
        to: AbsolutePath,
    },
    /// See [`OpCall::LeaseStatus`].
    LeaseStatus { lease: u64 },
    /// See [`OpCall::LeaseClose`].
    LeaseClose { lease: u64 },
}

/// What the session's reader hands its owner ([`Registration::accept`]), in the order the sandbox
/// sent it, ending with the session's end, once.
#[derive(Debug)]
#[must_use = "an arrival may hold an op, which dropped unanswered wedges the sandbox's ops, or a \
              model request, which dropped unanswered holds its client and its place in the \
              flight: answer each with `Session::reply`, `Session::refuse` or \
              `IncomingModelRequest::respond`"]
pub enum Arrival {
    /// An op that arrived whole, for the owner to answer.
    Op(IncomingOp),
    /// An op whose payload was over the session's limit, for the owner to refuse.
    Refused(RefusedOp),
    /// A model request that arrived whole, for the owner to answer.
    ModelRequest(IncomingModelRequest),
    /// The exit of the command [`Session::start`] returned `process` for.
    Exited {
        process: Process,
        status: ExitStatus,
    },
    /// The session's end, for `reason`, for which its requests fail too.
    Ended { reason: String },
}

/// An op whose payload was over the session's limit, which all arrived and was dropped, in
/// flight until [`Session::refuse`] answers it, on the session that handed it out: what a
/// record of the op needs.
#[derive(Debug)]
#[must_use = "an op dropped unanswered wedges the sandbox's ops, which wait on it for good: \
              answer it with `Session::refuse`"]
// Read through its methods, so that what a record of the op says, and the refusal that
// `Session::refuse` sends, are what the session saw.
pub struct RefusedOp {
    session: u64,
    /// The sandbox's number for the op, which the refusal names.
    number: u32,
    call: OpCall,
    size: u64,
    payload_limit: OpPayloadLimit,
}

impl RefusedOp {
    /// What the op asked: a call that takes a payload, an [`OpCall::Judge`] or an
    /// [`OpCall::Submit`].
    pub fn call(&self) -> &OpCall {
        &self.call
    }

    /// The payload's size, in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The limit the payload was over, which the session was accepted with.
    pub fn payload_limit(&self) -> OpPayloadLimit {
        self.payload_limit
    }

    /// Why the op is refused: the message that [`Session::refuse`] sends.
    pub fn message(&self) -> String {
        format!(
            "the payload is {} bytes, more than the limit of {1}: send at most {1}",
            self.size,
            self.payload_limit.bytes()
        )
    }
}

/// A request that a client in the sandbox made to its model endpoint, whole, and in flight until
/// its response ends.
#[derive(Debug)]
#[must_use = "a model request dropped unanswered holds its client, and its place in the flight, \
              until the session ends: answer it with `IncomingModelRequest::respond`"]
pub struct IncomingModelRequest {
    pub request: ModelRequest,
    responder: Responder,
}

/// An HTTP request to a sandbox's model endpoint, as its client sent it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRequest {
    pub method: String,
    /// The request's target: its path and any query, such as `/responses`.
    pub path: String,
    /// The headers, in the order the client sent them, names in lowercase.
    pub headers: Vec<(String, String)>,
    /// The body, of at most [`MAX_MODEL_REQUEST_BYTES`].
    pub body: Vec<u8>,
}

/// The response to a model request, streaming to its client by the deadline that
/// [`respond`](IncomingModelRequest::respond) gave. [`end`](Self::end) ends it whole;
/// [`cut`](Self::cut), or dropping it unended, cuts it short, so that its client doesn't take it
/// as whole.
#[derive(Debug)]
pub struct ModelResponse {
    responder: Responder,
    by: Deadline,
    /// Whether the response has ended, which takes its request out of flight.
    finished: bool,
}

#[derive(Debug)]
struct Responder {
    request: u32,
    // Shared with the session, its reader thread, and the responses to the other model requests.
    shared: Arc<Shared>,
}

/// A command started in a sandbox, which its exit names ([`Arrival::Exited`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Process {
    session: u64,
    /// The stream it was started on, which its exit comes on.
    stream: u32,
}

impl Session {
    fn new(
        stream: Connection,
        gpus: Option<Vec<GpuModel>>,
        limit: Option<OpPayloadLimit>,
        takes_models: bool,
        send: Box<dyn Fn(Arrival) + Send>,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            connection: stream,
            writer: Mutex::new(()),
            routes: Mutex::new(Routes {
                open: BTreeMap::new(),
                next_stream: 1,
                op: None,
                models: BTreeSet::new(),
            }),
            ended: OnceLock::new(),
        });
        let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
        let assembler = Assembler {
            owner: Owner { session: id, send },
            limit,
            open: None,
            shared: Arc::clone(&shared),
            takes_models,
            models: BTreeMap::new(),
        };
        // Reads the sandbox's frames while the owner writes requests and waits on their replies.
        let reader = thread::Builder::new()
            .name("session".to_owned())
            .spawn(move || route(assembler))
            .map_err(|error| context("the session's reader thread didn't start", &error))?;
        Ok(Self {
            id,
            shared,
            reader: Some(reader),
            gpus,
        })
    }

    /// The GPUs that the sandbox's `sandboxd` saw as it started, each by its model as
    /// `nvidia-smi` names it, if it could tell ([`Hello::gpus`](schema::sandbox::Hello::gpus)).
    pub fn gpus(&self) -> Option<&[GpuModel]> {
        self.gpus.as_deref()
    }

    /// Puts [`pumpkin_subdirs()`] under [`pumpkin_dir()`], with [`dir_mode()`], which every
    /// sandbox needs before its first command, by `by` ([deadlines](crate#deadlines)).
    pub fn set_up(&mut self, by: Deadline) -> Result<(), String> {
        let (root, subdirs) = (pumpkin_dir(), pumpkin_subdirs());
        let puts = subdirs.into_iter().map(|dir| Put::Dir(dir, dir_mode()));
        self.put(&root, puts, by).map_err(|error| {
            let failed =
                format!("the directories under `{root}` can't be put in the sandbox: {error}");
            // A deadline's error says what is expected.
            if error.kind() == io::ErrorKind::TimedOut {
                failed
            } else {
                format!(
                    "{failed}: expected its `sandboxd` to put them, as the role's user, who owns \
                     `{root}`"
                )
            }
        })
    }

    /// Writes `entries` below `dir` in the sandbox, creating `dir` and its missing parents.
    /// Each entry's parent is `dir` or comes earlier in `entries`, unless it exists already, and
    /// each gets its mode as it is made ([`tree::Writer::entry`]). Fails if `by` passes first
    /// ([deadlines](crate#deadlines)).
    pub fn put<'a>(
        &mut self,
        dir: &AbsolutePath,
        entries: impl IntoIterator<Item = Put<'a>>,
        by: Deadline,
    ) -> io::Result<()> {
        self.shared.put(dir, Root::Kept, entries, by)
    }

    /// Starts a command, whose exit arrives naming the [`Process`] this returns. Fails if `by`
    /// passes first ([deadlines](crate#deadlines)).
    pub fn start(&mut self, launch: Launch, by: Deadline) -> io::Result<Process> {
        let (stream, replies) = self.shared.open(Route::Start, by)?;
        let started = (self.shared)
            .send(&ToSandbox::Start { stream, launch }, &[], by)
            .and_then(|()| self.shared.done(&replies, by));
        match started {
            // The reader removes the command's route at its exit, which outlives this call.
            Ok(()) => Ok(Process {
                session: self.id,
                stream,
            }),
            Err(error) => {
                self.shared.close(stream);
                Err(error)
            }
        }
    }

    /// Kills `process`'s group ([`ToSandbox::Kill`]), whose exit then arrives as any other's.
    /// Fails if `by` passes first ([deadlines](crate#deadlines)).
    ///
    /// # Panics
    ///
    /// Panics if `process` was started on another session.
    pub fn kill(&mut self, process: Process, by: Deadline) -> io::Result<()> {
        let Process { session, stream } = process;
        if session != self.id {
            unreachable!(
                "the command on stream {stream} was killed on a session other than the one it was \
                 started on, though `kill`'s contract requires that one"
            )
        }
        self.shared.request(
            |request| ToSandbox::Kill {
                stream: request,
                command: stream,
            },
            by,
        )
    }

    /// Kills the sandbox's processes that [`ToSandbox::Freeze`] names, so that nothing collected
    /// afterwards changes, and returns once they are gone. Fails if `by` passes first
    /// ([deadlines](crate#deadlines)).
    pub fn freeze(&mut self, by: Deadline) -> io::Result<()> {
        self.shared
            .request(|stream| ToSandbox::Freeze { stream }, by)
    }

    /// Copies the tree in the sandbox's `dir` into `into`, a new directory, within `limits`.
    /// Fails if `into` exists, the tree passes a limit, or `by` passes first
    /// ([deadlines](crate#deadlines)). After any failure, `into` holds whatever arrived before it.
    pub fn collect(
        &mut self,
        dir: &AbsolutePath,
        into: &Path,
        limits: Limits,
        by: Deadline,
    ) -> io::Result<()> {
        let collected = self.shared.collect(dir, into, limits, by, &mut || false);
        collected.map(drop)
    }

    /// A handle through which another thread than the owner's moves trees in and out of the
    /// sandbox, while the owner drives the session as before.
    pub fn trees(&self) -> Trees {
        Trees {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Answers an op, promptly, as [the protocol](schema::sandbox) requires. Fails if `by` passes
    /// first ([deadlines](crate#deadlines)); a `reply` too long for a frame refuses the op
    /// instead, and fails.
    ///
    /// # Panics
    ///
    /// Panics if `op` arrived on another session.
    pub fn reply(&mut self, op: IncomingOp, reply: OpReply, by: Deadline) -> io::Result<()> {
        let IncomingOp {
            session,
            number,
            op,
        } = op;
        // Its payload goes first, so that the next op's buffer is never held beside it while the
        // write waits.
        drop(op);
        self.clear_flight(session, number);
        match self.answer(number, reply, by) {
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                let message = format!("the reply was too long to send: {error}");
                self.answer(number, OpReply::Refused { message }, by)?;
                Err(error)
            }
            answered => answered,
        }
    }

    /// Answers an op whose payload was over the limit with its refusal. Fails if `by` passes
    /// first ([deadlines](crate#deadlines)).
    ///
    /// # Panics
    ///
    /// Panics if `refused` arrived on another session.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "taking the op, as `reply` does, answers it once"
    )]
    pub fn refuse(&mut self, refused: RefusedOp, by: Deadline) -> io::Result<()> {
        let message = refused.message();
        let RefusedOp {
            session,
            number,
            call: _,
            size: _,
            payload_limit: _,
        } = refused;
        self.clear_flight(session, number);
        match self.answer(number, OpReply::Refused { message }, by) {
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => unreachable!(
                "the refusal of op {number} is too long, though its message is \
                 `RefusedOp::message`, which names only the payload's size, a `u64`, and the \
                 limit, so its frame's message is under 200 bytes, far under \
                 `frame::MAX_MESSAGE_BYTES`, 64 KiB: {error}"
            ),
            answered => answered,
        }
    }

    /// Takes op `number`, which arrived on session `session`, out of the flight, before its
    /// answer is written.
    fn clear_flight(&self, session: u64, number: u32) {
        // A lifetime brand tying each op to its session would reshape all of `Session`'s `pub`
        // API.
        if session != self.id {
            unreachable!(
                "op {number} was answered on a session other than the one it arrived on, though \
                 the contracts of `reply` and `refuse` require the one that handed it out"
            )
        }
        // Before the answer, since the sandbox may open the next op as soon as it reads one,
        // which a later clearing would drop.
        lock(&self.shared.routes).op = None;
    }

    /// Sends `reply` to op `number`, already out of the flight. A reply too long for a frame
    /// leaves the session open, so that the op can still be answered.
    fn answer(&mut self, number: u32, reply: OpReply, by: Deadline) -> io::Result<()> {
        self.shared.can_send(by, Due::Request)?;
        let message = ToSandbox::OpReply { op: number, reply };
        let writer = lock(&self.shared.writer);
        (self.shared).write(&writer, &message, &[], by, Due::Request)
    }
}

impl Shared {
    fn put<'a>(
        &self,
        dir: &AbsolutePath,
        root: Root,
        entries: impl IntoIterator<Item = Put<'a>>,
        by: Deadline,
    ) -> io::Result<()> {
        let (stream, replies) = self.open(Route::Reply, by)?;
        let put = self.send_tree(stream, dir, root, entries, &replies, by);
        self.close(stream);
        put
    }

    /// Collects as [`Session::collect`] does, cut short, with [`io::ErrorKind::Interrupted`],
    /// once `stopped`, which it asks before it takes each part, returns true, and returns the
    /// tree's entries.
    fn collect(
        &self,
        dir: &AbsolutePath,
        into: &Path,
        limits: Limits,
        by: Deadline,
        stopped: &mut dyn FnMut() -> bool,
    ) -> io::Result<Vec<Entry>> {
        let (stream, parts) = self.open(Route::Tree, by)?;
        let created = fs::create_dir(into).map_err(|error| context(into.display(), &error));
        let collected = created.and_then(|()| {
            let tree = tree::Writer::new(into.to_owned(), limits, Modes::Umasked);
            let dir = dir.clone();
            self.send(&ToSandbox::Collect { stream, dir }, &[], by)?;
            self.receive_tree(stream, &parts, tree, by, stopped)
        });
        // What `sandboxd` still sends of a tree cut short arrives on a closed stream, which the
        // reader drops.
        self.close(stream);
        collected
    }

    /// Sends the tree of `entries` on `stream`, and returns `sandboxd`'s reply on `replies`. A
    /// file that can't be read on the host ends the tree early, which `sandboxd` then fails, and
    /// fails the put, as does a failure `sandboxd` replies with before the tree's end.
    fn send_tree<'a>(
        &self,
        stream: u32,
        dir: &AbsolutePath,
        root: Root,
        entries: impl IntoIterator<Item = Put<'a>>,
        replies: &Receiver<Result<(), String>>,
        by: Deadline,
    ) -> io::Result<()> {
        let dir = dir.clone();
        self.send(&ToSandbox::Put { stream, dir, root }, &[], by)?;
        for entry in entries {
            // `sandboxd` replies before the end only to fail the tree, which then takes nothing
            // more.
            if let Ok(failed) = replies.try_recv() {
                self.send(&ToSandbox::End { stream }, &[], by)?;
                return failed.map_err(io::Error::other);
            }
            match entry {
                Put::Dir(path, mode) => {
                    let entry = Entry::Dir { path, mode };
                    self.send(&ToSandbox::Entry { stream, entry }, &[], by)?;
                }
                Put::File(path, mode, bytes) => {
                    let size = bytes.len() as u64;
                    let entry = Entry::File { path, size, mode };
                    self.send(&ToSandbox::Entry { stream, entry }, &[], by)?;
                    for chunk in bytes.chunks(frame::MAX_BODY_BYTES) {
                        self.send(&ToSandbox::Data { stream }, chunk, by)?;
                    }
                }
                Put::Read(path, mode, file) => {
                    if let Err(error) = self.send_file(stream, path, mode, file, by)? {
                        // The tree ends with the file short, which `sandboxd` fails.
                        self.send(&ToSandbox::End { stream }, &[], by)?;
                        let _ = self.receive(replies, by)?;
                        return Err(error);
                    }
                }
            }
        }
        self.send(&ToSandbox::End { stream }, &[], by)?;
        self.done(replies, by)
    }

    /// Sends `file`, at `path` in the tree on `stream`, with `mode`, reading it as it goes. The
    /// outer error is the session's; the inner one is the file's, read short or failing to be
    /// read, after which the tree is to end at once.
    fn send_file(
        &self,
        stream: u32,
        path: RelativePath,
        mode: Mode,
        file: fs::File,
        by: Deadline,
    ) -> io::Result<io::Result<()>> {
        let size = match file.metadata() {
            Ok(metadata) => metadata.len(),
            Err(error) => return Ok(Err(context(&path, &error))),
        };
        let shown = path.clone();
        let entry = Entry::File { path, size, mode };
        self.send(&ToSandbox::Entry { stream, entry }, &[], by)?;
        let (mut file, mut left) = (file.take(size), size);
        let mut buffer =
            vec![0; frame::MAX_BODY_BYTES.min(usize::try_from(size).unwrap_or(usize::MAX))];
        while left > 0 {
            let read = match file.read(&mut buffer) {
                Ok(0) => {
                    let error = io::Error::other(format!("{left} of its bytes are missing"));
                    return Ok(Err(context(&shown, &error)));
                }
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Ok(Err(context(&shown, &error))),
            };
            self.send(&ToSandbox::Data { stream }, &buffer[..read], by)?;
            left -= read as u64;
        }
        Ok(Ok(()))
    }

    /// Writes the tree whose parts arrive on `stream` with `tree`, until its end, and returns its
    /// entries, as they arrived.
    fn receive_tree(
        &self,
        stream: u32,
        parts: &Receiver<Result<Part, String>>,
        mut tree: tree::Writer,
        by: Deadline,
        stopped: &mut dyn FnMut() -> bool,
    ) -> io::Result<Vec<Entry>> {
        let mut entries = Vec::new();
        loop {
            if stopped() {
                let cut = "stopped before the tree was whole";
                return Err(io::Error::new(io::ErrorKind::Interrupted, cut));
            }
            match self.receive(parts, by)? {
                Ok(Part::Entry(entry)) => {
                    self.created(stream, tree.entry(&entry))?;
                    entries.push(entry);
                }
                Ok(Part::Data(bytes)) => self.written(stream, tree.data(&bytes))?,
                Ok(Part::End) => return self.written(stream, tree.finish()).map(|()| entries),
                Err(message) => return Err(io::Error::other(message)),
            }
        }
    }

    /// Returns `written`, what came of writing a part of the tree that arrives on `stream`,
    /// after ending the session if it shows the tree malformed.
    fn written(&self, stream: u32, written: io::Result<()>) -> io::Result<()> {
        match written {
            // A malformed tree breaks the protocol. The writer fails with `InvalidData` where the
            // parts don't fit together, as file data past its file's size, and no system error
            // has that kind. Any other error fails this collect alone: a limit passed,
            // `FileTooLarge`, is the request's own bound, not a break, and the host failing to
            // write is no fault of the sandbox's.
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                Err(self.malformed(stream, &error))
            }
            written => written,
        }
    }

    /// As [`written`](Self::written), for creating an entry.
    fn created(&self, stream: u32, created: io::Result<()>) -> io::Result<()> {
        match created {
            // Creating an entry fails with `NotFound`, `NotADirectory` or `AlreadyExists` where
            // its parent is missing or a file, or its path is taken. In `into`, new and left to
            // this collect, only the tree's own earlier entries can cause that, given that `into`
            // is on a filesystem that neither folds case nor normalizes names: on one that does,
            // a correct `sandboxd` that sends `A` and `a` would meet `AlreadyExists`, and end the
            // session. Only creating an entry names a path the tree chose; writing a file's bytes
            // goes to a file already open, so a system error there is the host's.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound
                        | io::ErrorKind::NotADirectory
                        | io::ErrorKind::AlreadyExists
                ) =>
            {
                Err(self.malformed(stream, &error))
            }
            created => self.written(stream, created),
        }
    }

    /// Ends the session, as the tree that arrives on `stream` is malformed, which `error` shows,
    /// and returns the request's error.
    fn malformed(&self, stream: u32, error: &io::Error) -> io::Error {
        let reason = broke(&format!(
            "the tree it sent on stream {stream} is malformed: {error}"
        ));
        self.end(reason);
        self.ended()
    }

    fn request(&self, message: impl FnOnce(u32) -> ToSandbox, by: Deadline) -> io::Result<()> {
        let (stream, replies) = self.open(Route::Reply, by)?;
        let done = self
            .send(&message(stream), &[], by)
            .and_then(|()| self.done(&replies, by));
        self.close(stream);
        done
    }

    /// Opens a stream for a request to send by `by`, whose replies `route` takes.
    fn open<R>(
        &self,
        route: fn(SyncSender<R>) -> Route,
        by: Deadline,
    ) -> io::Result<(u32, Receiver<R>)> {
        self.can_send(by, Due::Request)?;
        // One item in every queue. A put's, a freeze's or a start's queue holds its outcome,
        // done or failed, the one reply its request takes. A second outcome breaks the protocol
        // if it arrives before the first is taken, as it finds the queue full, or after a
        // start's `done` and before the command's exit, as it then reaches the running command,
        // whose stream carries only that exit, not the queue. Any other second outcome goes
        // unread: into the emptied queue until the request closes its stream, or onto the
        // closed stream after (see `route_reply`). A collect's queue holds its tree's parts, or
        // its failure, and one of slack lets the reader hand over a part it has read and go on
        // reading while the request writes the one before. The gateway then holds at most three
        // of a tree's parts, each holding at most a frame's 1 MiB body; more slack would hold
        // more memory without speeding a transfer that goes at the pace of the slower side.
        let (sender, replies) = mpsc::sync_channel(1);
        let mut routes = lock(&self.routes);
        // Again under the lock, as the session may have ended since: `end` says why.
        if self.ended.get().is_some() {
            return Err(self.ended());
        }
        let stream = routes.next_stream;
        routes.next_stream = stream.wrapping_add(1);
        routes.open.insert(stream, route(sender));
        Ok((stream, replies))
    }

    fn close(&self, stream: u32) {
        lock(&self.routes).open.remove(&stream);
    }

    /// Sends a frame. Any failure ends the session: one frame of a request that spans several,
    /// such as a [`put`](Self::put)'s, may be too long after the first was sent.
    fn send(&self, message: &ToSandbox, body: &[u8], by: Deadline) -> io::Result<()> {
        let writer = lock(&self.writer);
        match self.write(&writer, message, body, by, Due::Request) {
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                Err(self.write_failed(&error, Due::Request))
            }
            sent => sent,
        }
    }

    fn receive<R>(&self, replies: &Receiver<R>, by: Deadline) -> io::Result<R> {
        // The gateway bounds no wait of its own. A stall the connection rides out may be
        // latency, which only the operation the request serves knows how long to wait out, as
        // its deadline says; a sandbox that is gone closes its connection, which ends the
        // session at once. A deadline that has passed fails at once, even with a frame waiting,
        // so that a tree sent faster than it is written ends by it too.
        let received = match by.remaining() {
            Some(left) => replies.recv_timeout(left),
            None => Err(RecvTimeoutError::Timeout),
        };
        match received {
            Ok(reply) => Ok(reply),
            Err(RecvTimeoutError::Timeout) => Err(self.timed_out(
                "the sandbox didn't answer before the request's deadline: expected its answer \
                 while the deadline lasts"
                    .to_owned(),
            )),
            Err(RecvTimeoutError::Disconnected) => Err(self.ended()),
        }
    }

    fn done(&self, replies: &Receiver<Result<(), String>>, by: Deadline) -> io::Result<()> {
        self.receive(replies, by)?.map_err(io::Error::other)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Ending the session shuts the connection both ways, which wakes the reader thread's
        // read and fails the sandbox's sends from then on, so the reader reads only what it had
        // not yet read before it ends.
        let reason = "the owner dropped the session".to_owned();
        self.shared.end(reason);
        // Nothing else holds the reader up: its sends to the owner never block; a tree's frames
        // wait only for a collect, which has returned, dropping their queue, before anything can
        // drop the session, or which takes them on another thread, through `Trees`; and it takes
        // the routes' lock only around steps that never block, as every holder does, and this
        // thread holds it no longer.
        if let Some(reader) = self.reader.take()
            && reader.join().is_err()
        {
            unreachable!(
                "the session's reader thread ended in a panic, though a panic aborts the \
                 process (`stdx::abort_on_panic`)"
            )
        }
    }
}

impl IncomingModelRequest {
    /// Starts the response with `status` and `headers`, and returns it, to stream its body by
    /// `by` ([deadlines](crate#deadlines)). A response that hasn't ended by `by` ends the session
    /// at its next frame, or as it drops. Fails if the head is too long for a frame, which cuts
    /// the request short without a response.
    pub fn respond(
        self,
        status: u16,
        headers: Vec<(String, String)>,
        by: Deadline,
    ) -> io::Result<ModelResponse> {
        let Self {
            request: _,
            responder,
        } = self;
        let request = responder.request;
        let response = ModelResponse {
            responder,
            by,
            finished: false,
        };
        let head = ToSandbox::ModelResponse {
            request,
            status,
            headers,
        };
        // A response that fails to start drops here, cut short.
        response.responder.send(&head, &[], by)?;
        Ok(response)
    }
}

impl ModelResponse {
    /// Sends `bytes` of the body on to the client.
    pub fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        let request = self.responder.request;
        for chunk in bytes.chunks(frame::MAX_BODY_BYTES) {
            self.responder
                .send(&ToSandbox::ModelData { request }, chunk, self.by)?;
        }
        Ok(())
    }

    /// Ends the response whole.
    pub fn end(mut self) -> io::Result<()> {
        self.finish(false)
    }

    /// Ends the response cut short, so that its client doesn't take it as whole.
    pub fn cut(mut self) -> io::Result<()> {
        self.finish(true)
    }

    /// Ends the response, `cut` short or not. Its request leaves the flight first, since the
    /// sandbox may open another as soon as the end reaches it.
    fn finish(&mut self, cut: bool) -> io::Result<()> {
        self.finished = true;
        let request = self.responder.request;
        lock(&self.responder.shared.routes).models.remove(&request);
        self.responder
            .send(&ToSandbox::ModelEnd { request, cut }, &[], self.by)
    }
}

impl Drop for ModelResponse {
    fn drop(&mut self) {
        if !self.finished {
            // A session that has ended has no client left to tell.
            let _ = self.finish(true);
        }
    }
}

impl Responder {
    /// Sends a frame of the response by `by`, as [`Shared::write`] does, once the session's owner
    /// and the other responses let it write.
    fn send(&self, message: &ToSandbox, body: &[u8], by: Deadline) -> io::Result<()> {
        let writer = lock(&self.shared.writer);
        self.shared.can_send(by, Due::Response)?;
        (self.shared).write(&writer, message, body, by, Due::Response)
    }
}

/// Whose deadline a frame is written by, which the reason for the session's end names if it
/// passes: a request's, which the owner's reply to an op shares, or a response's.
#[derive(Clone, Copy, Debug)]
enum Due {
    Request,
    Response,
}

impl Due {
    fn noun(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Response => "response",
        }
    }
}

impl Shared {
    /// Fails before a frame due by `by` is sent if the session has ended, for its reason, or else
    /// if `by` has passed, which ends the session, though the sandbox has had no part in it.
    fn can_send(&self, by: Deadline, due: Due) -> io::Result<()> {
        if self.ended.get().is_some() {
            return Err(self.ended());
        }
        if by.remaining().is_none() {
            let reason = format!(
                "the {}'s deadline passed before it was sent: expected time left to send it",
                due.noun()
            );
            return Err(self.timed_out(reason));
        }
        Ok(())
    }

    /// Ends the session for `reason`, as a deadline has passed, and returns the error of the
    /// request or the response it was due by.
    fn timed_out(&self, reason: String) -> io::Error {
        self.end(reason.clone());
        io::Error::new(io::ErrorKind::TimedOut, reason)
    }

    /// Writes a frame by `by` while the caller holds the writer's lock. A frame too long, which
    /// `frame::write` refuses before writing anything, fails with
    /// [`io::ErrorKind::InvalidInput`] and leaves the session open; any other failure ends it
    /// ([`write_failed`](Self::write_failed)).
    fn write(
        &self,
        _writer: &MutexGuard<'_, ()>,
        message: &ToSandbox,
        body: &[u8],
        by: Deadline,
        due: Due,
    ) -> io::Result<()> {
        let connection = &self.connection;
        match frame::write(&mut Until { connection, by }, message, body) {
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => Err(error),
            written => written.map_err(|error| self.write_failed(&error, due)),
        }
    }

    /// Ends the session after a write of a frame due by `due`'s deadline that failed with
    /// `error`, which may leave part of a frame on the connection, and returns the writer's
    /// error: the deadline's, if it has passed, or else the session's, whose reason is the failed
    /// write's unless it had ended already.
    fn write_failed(&self, error: &io::Error, due: Due) -> io::Error {
        if passed(error) {
            let reason = format!(
                "the sandbox didn't take what was sent before the {}'s deadline: expected it to \
                 read while the deadline lasts",
                due.noun()
            );
            return self.timed_out(reason);
        }
        let reason = if closed_by_sandbox(error) {
            SANDBOX_ENDED.to_owned()
        } else {
            format!("writing to the sandbox failed: {error}")
        };
        self.end(reason);
        self.ended()
    }

    /// Ends the session, unless it has ended already: closes the connection, which stops the
    /// sandbox, records why, and wakes every request waiting on it. Returns the reason recorded:
    /// `reason`, unless the session had ended already.
    fn end(&self, reason: String) -> &str {
        let mut routes = lock(&self.routes);
        // Closed first, under the lock, so that no write reaches the sandbox once anything can
        // learn the session ended: an owner woken by the end may still answer an op. A connection
        // already closed is fine. The reason is set under the lock too, so that no route opens
        // after the routes are cleared. A reason already set is the session's.
        let _ = self.connection.shutdown();
        let recorded = self.ended.get_or_init(|| reason);
        routes.open.clear();
        recorded
    }

    /// The error of a request on a session that has ended, for the reason recorded.
    fn ended(&self) -> io::Error {
        // Sending the reason as each channel's last message would need room in queues that may be
        // full.
        let Some(reason) = self.ended.get() else {
            unreachable!(
                "the session ended without a reason, though every way to learn that it ended \
                 follows the reason's recording: `end` records it before it closes the routes, \
                 which close otherwise only after a frame that ends their request or once nothing \
                 receives from them; and the rest call `end` or check the reason first"
            )
        };
        io::Error::new(io::ErrorKind::ConnectionAborted, reason.clone())
    }
}

/// Whether `error`, from a write through [`Until`], is for its deadline's passing.
fn passed(error: &io::Error) -> bool {
    // A socket's timeout fails a call as it would fail a non-blocking one.
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// Writes to a connection until `by`, after which every write fails as timed out.
struct Until<'a> {
    connection: &'a Connection,
    by: Deadline,
}

impl Write for Until<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Set before each write, since the socket's timeout bounds each call on its own. A
        // deadline that has passed fails here, not in the socket, whose `None` would mean no
        // timeout at all.
        let Some(left) = self.by.remaining() else {
            return Err(io::ErrorKind::TimedOut.into());
        };
        self.connection.set_write_timeout(Some(left))?;
        (&mut &*self.connection).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&mut &*self.connection).flush()
    }
}

/// A session's connection, which behaves alike over each transport.
#[derive(Debug)]
enum Connection {
    Unix(UnixStream),
    Tcp(TcpStream),
    Tls(tls::Stream),
}

impl Connection {
    fn shutdown(&self) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.shutdown(Shutdown::Both),
            Self::Tcp(stream) => stream.shutdown(Shutdown::Both),
            Self::Tls(stream) => stream.shutdown(),
        }
    }

    fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.set_nonblocking(nonblocking),
            Self::Tcp(stream) => stream.set_nonblocking(nonblocking),
            Self::Tls(stream) => stream.set_nonblocking(nonblocking),
        }
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.set_write_timeout(timeout),
            Self::Tcp(stream) => stream.set_write_timeout(timeout),
            Self::Tls(stream) => stream.set_write_timeout(timeout),
        }
    }
}

// Through a shared reference, as `UnixStream` and `TcpStream` read and write, since the session's
// reader thread reads beside the owner that writes and may end the session.
impl Read for &Connection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Connection::Unix(stream) => (&mut &*stream).read(buf),
            Connection::Tcp(stream) => (&mut &*stream).read(buf),
            Connection::Tls(stream) => (&mut &*stream).read(buf),
        }
    }
}

impl Write for &Connection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Connection::Unix(stream) => (&mut &*stream).write(buf),
            Connection::Tcp(stream) => (&mut &*stream).write(buf),
            Connection::Tls(stream) => (&mut &*stream).write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Connection::Unix(stream) => (&mut &*stream).flush(),
            Connection::Tcp(stream) => (&mut &*stream).flush(),
            Connection::Tls(stream) => (&mut &*stream).flush(),
        }
    }
}

/// Reads frames from the sandbox until the session ends: each reply goes to the request
/// waiting on its stream, the frames of each op and model request to `ops`, and each command's
/// exit, then the end, to the owner.
fn route(mut ops: Assembler) {
    let shared = Arc::clone(&ops.shared);
    let routes = &shared.routes;
    let mut reader = BufReader::new(&shared.connection);
    let reason = loop {
        let (message, body) = match frame::read::<FromSandbox>(&mut reader) {
            Ok(Some(frame)) => frame,
            Ok(None) => break SANDBOX_ENDED.to_owned(),
            Err(error) if closed_by_sandbox(&error) => break SANDBOX_ENDED.to_owned(),
            Err(error) => break format!("the session failed: {error}"),
        };
        let outcome = match message {
            FromSandbox::Op { op, call, size } => ops.open(op, call, size),
            FromSandbox::OpData { op } => ops.data(op, &body),
            FromSandbox::ModelRequest {
                request,
                method,
                path,
                headers,
                size,
            } => {
                let head = ModelRequest {
                    method,
                    path,
                    headers,
                    body: Vec::new(),
                };
                ops.open_model(request, head, size)
            }
            FromSandbox::ModelData { request } => ops.model_data(request, &body),
            FromSandbox::Done { stream } => route_reply(routes, stream, Reply::Done, &ops.owner),
            FromSandbox::Failed { stream, message } => {
                route_reply(routes, stream, Reply::Failed(message), &ops.owner)
            }
            FromSandbox::Exited { stream, status } => {
                route_reply(routes, stream, Reply::Exited(status), &ops.owner)
            }
            FromSandbox::Entry { stream, entry } => {
                route_reply(routes, stream, Reply::Part(Part::Entry(entry)), &ops.owner)
            }
            FromSandbox::Data { stream } => {
                route_reply(routes, stream, Reply::Part(Part::Data(body)), &ops.owner)
            }
            FromSandbox::End { stream } => {
                route_reply(routes, stream, Reply::Part(Part::End), &ops.owner)
            }
        };
        if let Err(reason) = outcome {
            break reason;
        }
    };
    // Last, since only this thread reaches the owner, and with the reason the session recorded
    // first, such as a request's deadline's, whose closing of the connection ended the read.
    let reason = shared.end(reason).to_owned();
    ops.owner.arrived(Arrival::Ended { reason });
}

/// The session's owner, as the reader thread reaches it.
struct Owner {
    session: u64,
    send: Box<dyn Fn(Arrival) + Send>,
}

impl Owner {
    fn arrived(&self, arrival: Arrival) {
        (self.send)(arrival);
    }
}

/// Hands a reply to the request waiting on `stream`, or a started command's exit to `owner`, or
/// drops it if `stream` has closed. Removes the route after the command's exit, or once its
/// receiver is gone. Fails if no request has opened `stream`, or if its route takes no such
/// reply, or no more.
fn route_reply(
    routes: &Mutex<Routes>,
    stream: u32,
    reply: Reply,
    owner: &Owner,
) -> Result<(), String> {
    let more = || {
        broke(&format!(
            "it sent more on stream {stream} than a request takes"
        ))
    };
    let mut locked = lock(routes);
    let Some(route) = locked.open.get_mut(&stream) else {
        if !(1..locked.next_stream).contains(&stream) {
            let expected = "nothing on a stream no request has opened";
            return Err(out_of_turn(stream, &reply, expected));
        }
        // A request closes its stream as it returns, even if it gave up before `sandboxd` was
        // done answering, as a collect does at a limit its tree passes, or any request at its
        // deadline. `sandboxd` can't know, and may still be sending what it had begun, so a
        // reply on a closed stream is an expected race, not a break.
        return Ok(());
    };
    let given_up = match route {
        Route::Reply(sender) | Route::Start(sender) => {
            let answer = match reply {
                Reply::Done => Ok(()),
                Reply::Failed(message) => Err(message),
                Reply::Exited(_) | Reply::Part(_) => {
                    return Err(out_of_turn(stream, &reply, "`done` or `failed`"));
                }
            };
            let sender = sender.clone();
            // A command runs once its start is done, and its stream then carries only its exit.
            if matches!(route, Route::Start(_)) && answer.is_ok() {
                *route = Route::Running;
            }
            drop(locked);
            match sender.try_send(answer) {
                Ok(()) => false,
                Err(TrySendError::Disconnected(_)) => true,
                Err(TrySendError::Full(_)) => return Err(more()),
            }
        }
        Route::Tree(sender) => {
            let part = match reply {
                Reply::Part(part) => Ok(part),
                Reply::Failed(message) => Err(message),
                Reply::Done | Reply::Exited(_) => {
                    let expected = "`entry`, `data`, `end` or `failed`";
                    return Err(out_of_turn(stream, &reply, expected));
                }
            };
            let sender = sender.clone();
            drop(locked);
            // The request writes the tree as it reads it, or drops the queue when it fails.
            sender.send(part).is_err()
        }
        Route::Running => {
            let status = match reply {
                Reply::Exited(status) => status,
                Reply::Done | Reply::Failed(_) | Reply::Part(_) => return Err(more()),
            };
            locked.open.remove(&stream);
            drop(locked);
            let process = Process {
                session: owner.session,
                stream,
            };
            owner.arrived(Arrival::Exited { process, status });
            false
        }
    };
    if given_up {
        lock(routes).open.remove(&stream);
    }
    Ok(())
}

/// Assembles the op and the model requests the sandbox sends, on the reader thread, and passes
/// each on whole.
struct Assembler {
    owner: Owner,
    /// The most bytes an op's payload may hold, if the session takes ops.
    limit: Option<OpPayloadLimit>,
    open: Option<(u32, Assembly)>,
    takes_models: bool,
    // Shared with the session, and handed to the response to each model request.
    shared: Arc<Shared>,
    /// The model requests whose bodies are arriving, by number, each with the bytes still due.
    models: BTreeMap<u32, (ModelRequest, u64)>,
}

struct Assembly {
    call: OpCall,
    /// The payload's bytes still to arrive: the op is whole once none are.
    due: u64,
    payload: Payload,
}

/// Where an arriving payload's bytes go.
enum Payload {
    /// Into a buffer that holds the whole payload, allocated as the op opened.
    Kept(Vec<u8>),
    /// Nowhere, since the payload's `size` is over the session's `limit`: the op is refused
    /// once all of it has arrived.
    Dropped { size: u64, limit: OpPayloadLimit },
}

impl Assembler {
    /// Puts op `number`, whose payload is `size` bytes, in flight, if the session
    /// takes ops and none is in flight, and passes it on at once if it has no payload.
    fn open(&mut self, number: u32, call: OpCall, size: u64) -> Result<(), String> {
        let Some(limit) = self.limit else {
            return Err(broke(&format!(
                "it opened op {number}, but the session takes no ops"
            )));
        };
        let mut routes = lock(&self.shared.routes);
        if let Some(in_flight) = routes.op {
            return Err(broke(&format!(
                "it opened op {number} while op {in_flight} is in flight"
            )));
        }
        let payload = payload(number, &call, size, limit)?;
        routes.op = Some(number);
        drop(routes);
        let assembly = Assembly {
            call,
            due: size,
            payload,
        };
        if size == 0 {
            self.pass_on(number, assembly);
        } else {
            self.open = Some((number, assembly));
        }
        Ok(())
    }

    /// Adds bytes to op `number`'s payload, or drops them if it is over the limit, and passes
    /// the op on once the payload's last byte has arrived.
    fn data(&mut self, number: u32, bytes: &[u8]) -> Result<(), String> {
        let Some((_, mut assembly)) = self.open.take().filter(|(open, _)| *open == number) else {
            return Err(broke(&format!(
                "it sent data for op {number}, which is not open"
            )));
        };
        let len = bytes.len() as u64;
        let Some(due) = assembly.due.checked_sub(len) else {
            return Err(broke(&format!(
                "it sent {len} bytes of op {number}'s payload, past its size: {} were left",
                assembly.due
            )));
        };
        assembly.due = due;
        match &mut assembly.payload {
            Payload::Kept(payload) => payload.extend_from_slice(bytes),
            Payload::Dropped { .. } => {}
        }
        if due == 0 {
            self.pass_on(number, assembly);
        } else {
            self.open = Some((number, assembly));
        }
        Ok(())
    }

    /// Passes op `number` on to the session's owner, now that its payload has all arrived:
    /// whole, or refused.
    fn pass_on(&self, number: u32, assembly: Assembly) {
        let Assembly {
            call,
            due: _,
            payload,
        } = assembly;
        let session = self.owner.session;
        self.owner.arrived(match payload {
            Payload::Kept(payload) => Arrival::Op(IncomingOp {
                session,
                number,
                op: whole(call, payload),
            }),
            Payload::Dropped { size, limit } => Arrival::Refused(RefusedOp {
                session,
                number,
                call,
                size,
                payload_limit: limit,
            }),
        });
    }

    /// Puts model request `number`, `head` with a body of `size` bytes to follow, in flight, if the
    /// session takes model requests and it breaks no other rule, and passes it on at once if it
    /// has no body.
    fn open_model(&mut self, number: u32, head: ModelRequest, size: u64) -> Result<(), String> {
        if !self.takes_models {
            return Err(broke(&format!(
                "it opened model request {number}, but the session takes no model requests: send \
                 one only on a session accepted to take them"
            )));
        }
        if size > MAX_MODEL_REQUEST_BYTES {
            return Err(broke(&format!(
                "it opened model request {number} with a body of {size} bytes, more than the \
                 protocol's {MAX_MODEL_REQUEST_BYTES}"
            )));
        }
        // The name itself stays unquoted, as it may be anything the sandbox chose.
        if (head.headers.iter()).any(|(name, _)| name.bytes().any(|b| b.is_ascii_uppercase())) {
            return Err(broke(&format!(
                "it opened model request {number} with a header name that is not in lowercase"
            )));
        }
        let mut flight = lock(&self.shared.routes);
        if flight.models.contains(&number) {
            return Err(broke(&format!(
                "it opened model request {number}, which is in flight"
            )));
        }
        // The session's model requests in its owner's inbox are all in flight, so this bounds
        // how many it leaves there, as `Registration::accept` counts on.
        if flight.models.len() >= MAX_MODEL_REQUESTS_IN_FLIGHT {
            return Err(broke(&format!(
                "it opened model request {number} while {MAX_MODEL_REQUESTS_IN_FLIGHT} are in \
                 flight, the most the protocol allows"
            )));
        }
        flight.models.insert(number);
        drop(flight);
        let Ok(capacity) = usize::try_from(size) else {
            unreachable!(
                "a model request's body of {size} bytes overflows a usize, though it is at most \
                 `MAX_MODEL_REQUEST_BYTES`, 64 MiB, which fits the usize of every Linux target, \
                 of 32 bits or more"
            )
        };
        let request = ModelRequest {
            body: Vec::with_capacity(capacity),
            ..head
        };
        if size == 0 {
            self.pass_on_model(number, request);
        } else {
            self.models.insert(number, (request, size));
        }
        Ok(())
    }

    /// Adds bytes to model request `number`'s body, and passes the request on once the body's
    /// last byte has arrived.
    fn model_data(&mut self, number: u32, bytes: &[u8]) -> Result<(), String> {
        // Bytes for a request never opened, or for one whose body has all arrived, break the
        // protocol. Unlike a reply on a stream that its request has closed, which `route_reply`
        // drops, they can't be the late end of an answer the gateway gave up on: a body ends at
        // the size the sandbox itself gave.
        let Some((mut request, due)) = self.models.remove(&number) else {
            return Err(broke(&format!(
                "it sent data for model request {number}, which is not open"
            )));
        };
        let len = bytes.len() as u64;
        let Some(due) = due.checked_sub(len) else {
            return Err(broke(&format!(
                "it sent {len} bytes of model request {number}'s body, past its size: {due} were \
                 left"
            )));
        };
        request.body.extend_from_slice(bytes);
        if due == 0 {
            self.pass_on_model(number, request);
        } else {
            self.models.insert(number, (request, due));
        }
        Ok(())
    }

    /// Passes model request `number` on to the session's owner, now that its body has all
    /// arrived, with the way to respond to it.
    fn pass_on_model(&self, number: u32, request: ModelRequest) {
        let responder = Responder {
            request: number,
            shared: Arc::clone(&self.shared),
        };
        let incoming = IncomingModelRequest { request, responder };
        self.owner.arrived(Arrival::ModelRequest(incoming));
    }
}

/// Where the bytes of op `number`'s payload, of `size` bytes, go: into a buffer of that size,
/// if it is within `limit`, or nowhere. Fails if the size is over the protocol's
/// [`MAX_OP_PAYLOAD_BYTES`], which `sandboxd` never sends, or if `call` takes no payload but
/// the size gives one.
fn payload(
    number: u32,
    call: &OpCall,
    size: u64,
    limit: OpPayloadLimit,
) -> Result<Payload, String> {
    if size > MAX_OP_PAYLOAD_BYTES {
        return Err(broke(&format!(
            "it opened op {number} with a payload of {size} bytes, more than the protocol's \
             {MAX_OP_PAYLOAD_BYTES}"
        )));
    }
    if size > 0 && !call.takes_payload() {
        return Err(broke(&format!(
            "it opened op {number} with a payload of {size} bytes, for a call that takes none"
        )));
    }
    // An op's payload goes into a buffer of the size the op declares. A payload over the
    // limit is refused only once all its bytes have arrived, so that the sandbox's next op
    // follows it whole.
    if size > limit.bytes() {
        return Ok(Payload::Dropped { size, limit });
    }
    let Ok(capacity) = usize::try_from(size) else {
        unreachable!(
            "a payload of {size} bytes overflows a usize, though it is at most the limit, which \
             is at most `MAX_OP_PAYLOAD_BYTES`, 64 MiB (an `OpPayloadLimit` holds no more), which \
             fits the usize of every Linux target, of 32 bits or more"
        )
    };
    Ok(Payload::Kept(Vec::with_capacity(capacity)))
}

/// The op that `call` makes with `payload`, which is empty for a call that takes none, since
/// [`payload`] refuses a size for one.
fn whole(call: OpCall, payload: Vec<u8>) -> Op {
    match call {
        OpCall::Ops => Op::Ops,
        OpCall::Judge { grant } => Op::Judge { grant, payload },
        OpCall::Submit { grant, finalize } => Op::Submit {
            grant,
            finalize,
            payload,
        },
        OpCall::Status { job } => Op::Status { job },
        OpCall::LeaseOpen {
            grant,
            instance_seconds,
        } => Op::LeaseOpen {
            grant,
            instance_seconds,
        },
        OpCall::LeaseRun {
            lease,
            argv,
            timeout_seconds,
        } => Op::LeaseRun {
            lease,
            argv,
            timeout_seconds,
        },
        OpCall::LeasePut {
            lease,
            tree,
            to,
            replace,
        } => Op::LeasePut {
            lease,
            tree,
            to,
            replace,
        },
        OpCall::LeaseGet { lease, from, to } => Op::LeaseGet { lease, from, to },
        OpCall::LeaseStatus { lease } => Op::LeaseStatus { lease },
        OpCall::LeaseClose { lease } => Op::LeaseClose { lease },
    }
}

/// Why a session ended that the sandbox ended, by closing its end of the connection.
const SANDBOX_ENDED: &str = "the sandbox ended the session";

/// Whether `error`, of a read from the connection or a write to it, says that the sandbox
/// closed its end.
fn closed_by_sandbox(error: &io::Error) -> bool {
    // A sandbox that closes its end with what the gateway sent still unread resets the
    // connection rather than ends it cleanly, which a read then fails with, and a write once
    // the reset has come, as a broken pipe; which of these the gateway meets first is a race.
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
    )
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    stdx::unpoisoned(mutex.lock())
}

/// `error`, prefixed with what failed, as `<what>: <error>`, keeping its kind.
fn context(what: impl fmt::Display, error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{what}: {error}"))
}

/// Why a session ends when the sandbox does `what`.
fn broke(what: &str) -> String {
    format!("the sandbox broke the protocol: {what}")
}

/// Why a session ends when the sandbox sends `reply` on `stream`, where only what `expected`
/// names may come.
fn out_of_turn(stream: u32, reply: &Reply, expected: &str) -> String {
    broke(&format!(
        "it sent `{}` on stream {stream} out of turn: expected {expected}",
        reply.name()
    ))
}
