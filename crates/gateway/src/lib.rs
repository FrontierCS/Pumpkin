//! The host's end of the sessions that sandboxes open, by [the protocol](schema::sandbox).
//!
//! A [`Registration`] meets one sandbox and becomes a [`Session`], which one owner drives: it puts
//! and collects trees, starts commands, and hears of whatever the sandbox sends, as the
//! [`Arrival`]s that the session's reader thread hands it. What an op or a model request means is
//! the owner's to decide: the gateway authorizes nothing, but bounds what they cost the host. The
//! owner answers an op with [`Session::reply`] or [`Session::refuse`], and a model request with
//! [`IncomingModelRequest::respond`], which may run on any thread.
//!
//! A sandbox, however buggy, can't harm the host through its session: a message that fails to
//! parse or comes out of turn, more replies than a waiting request takes, and a malformed tree
//! end the session, which stops the sandbox. A late reply, on a stream whose request has given
//! up, is dropped. A collected tree is written into a new directory of its own, within
//! [`Limits`]: a tree that passes them fails only its collect.
//!
//! # Deadlines
//!
//! Every request waits on the sandbox until its caller's deadline, so a slow link makes it wait,
//! never fail. The caller gives the deadline as `left`, a function that gives the time left by a
//! clock of the caller's, as [`Deadline::remaining`](stdx::Deadline::remaining) gives it by the
//! machine's: none once it has passed. A request whose deadline passes fails with
//! [`io::ErrorKind::TimedOut`] and ends the session; so does a sandbox that closes its connection,
//! at once. A frame may also wait behind another writer's, until that writer's deadline.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::{fmt, fs};

use bound::{Bound, Left, POLL, Until, passed};
pub use meeting::{Dialer, GatewayAddress, Listener, Registration, Sockets, Unattested, Unreached};
pub use op::Op;
pub use process::Process;
use schema::ir::{AbsolutePath, GpuModel, Mode};
use schema::sandbox::op::{OpCall, OpReply};
use schema::sandbox::tree::Limits;
use schema::sandbox::{
    Entry, ExitStatus, FromSandbox, MAX_MODEL_REQUEST_BYTES, MAX_MODEL_REQUESTS_IN_FLIGHT,
    MAX_OP_PAYLOAD_BYTES, OpPayloadLimit, RelativePath, Root, ToSandbox, dir_mode, pumpkin_dir,
    pumpkin_subdirs,
};

mod bound;
mod meeting;
mod op;
mod process;
mod trees;

// Tells one session's handles from another's: `kill`, `reply` and `refuse` check it. Every
// thread that makes a session shares the count.
static NEXT_SESSION: AtomicU64 = AtomicU64::new(0);

/// One sandbox's session, driven by its owner.
///
/// Once the session has ended, every call fails with `ConnectionAborted` and the reason. It ends
/// when the sandbox ends it, when the sandbox breaks the protocol, when a call's deadline passes
/// ([deadlines](crate#deadlines)), and when the owner drops it, which stops the sandbox and
/// returns once the reader has sent its last arrival, the end. That waits for a collect through
/// [`Trees`] to take the part the reader holds.
#[derive(Debug)]
pub struct Session {
    id: u64,
    // Shared with the reader thread, and with the responses to model requests, which stream from
    // the threads that serve them while the owner sends requests.
    shared: Arc<Shared>,
    reader: Option<JoinHandle<()>>,
    gpus: Option<Vec<GpuModel>>,
}

#[derive(Debug)]
struct Shared {
    // Every writer writes it and the reader reads it, and it ends the session without waiting for
    // a writer.
    connection: Connection,
    // Held for a whole frame, so that the frames of different threads don't interleave.
    writer: Mutex<()>,
    routes: Mutex<Routes>,
    // Why the session ended, recorded once, under the routes' lock, which `end` explains.
    ended: OnceLock<String>,
}

#[derive(Debug)]
struct Routes {
    // The requests that wait for the sandbox, by the stream each opened.
    open: BTreeMap<u32, Route>,
    // Requests open streams in order from 1, so every stream from 1 below it has been opened.
    next_stream: u32,
    // The op in flight: a session has one at most.
    op: Option<u32>,
    models: BTreeSet<u32>,
}

// Where the frames on a stream go. Each route takes only the replies its variant is for: any other
// breaks the protocol. A `Start` becomes `Running` once its command has started, and its stream
// then carries only the command's exit.
#[derive(Debug)]
enum Route {
    Reply(SyncSender<Result<(), String>>),
    Start(SyncSender<Result<(), String>>),
    Tree(SyncSender<Result<Part, String>>),
    Running,
}

enum Reply {
    Done,
    Failed(String),
    Exited(ExitStatus),
    Part(Part),
}

enum Part {
    Entry(Entry),
    Data(Vec<u8>),
    End,
}

impl Reply {
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

/// An entry of a tree to put.
#[derive(Debug)]
pub enum Put<'a> {
    Dir(RelativePath, Mode),
    File(RelativePath, Mode, &'a [u8]),
    /// A file of the host's, read as it is sent, whose size is its size as the put reaches it: one
    /// that then reads short, or fails to be read, fails the put, and the session lives on.
    Read(RelativePath, Mode, fs::File),
}

/// Moves trees through a session from other threads, beside its owner.
#[derive(Debug)]
pub struct Trees {
    shared: Arc<Shared>,
}

#[derive(Debug)]
#[must_use = "an op dropped unanswered wedges the sandbox's ops, which wait on it for good: \
              answer it with `Session::reply`"]
pub struct IncomingOp {
    session: u64,
    number: u32,
    pub op: Op,
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
    /// The exit of the command that [`Session::start`] returned `process` for.
    Exited {
        process: Process,
        status: ExitStatus,
    },
    /// The session's end, for `reason`.
    Ended { reason: String },
}

/// An op whose payload was over the session's limit, which all arrived and was dropped, in flight
/// until [`Session::refuse`] answers it, on the session that handed it out.
#[derive(Debug)]
#[must_use = "an op dropped unanswered wedges the sandbox's ops, which wait on it for good: \
              answer it with `Session::refuse`"]
pub struct RefusedOp {
    session: u64,
    number: u32,
    call: OpCall,
    size: u64,
    payload_limit: OpPayloadLimit,
}

impl RefusedOp {
    pub fn call(&self) -> &OpCall {
        &self.call
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn payload_limit(&self) -> OpPayloadLimit {
        self.payload_limit
    }

    pub fn message(&self) -> String {
        format!(
            "the payload is {} bytes, more than the limit of {1}: send at most {1}",
            self.size,
            self.payload_limit.bytes()
        )
    }
}

#[derive(Debug)]
#[must_use = "a model request dropped unanswered holds its client, and its place in the flight, \
              until the session ends: answer it with `IncomingModelRequest::respond`"]
pub struct IncomingModelRequest {
    pub request: ModelRequest,
    responder: Responder,
}

/// An HTTP request to a sandbox's model endpoint, as its client sent it, whole.
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
/// [`respond`](IncomingModelRequest::respond) gave.
///
/// A response that hasn't ended by then ends the session at its next frame, or as it drops.
/// [`end`](Self::end) ends it whole; [`cut`](Self::cut), or dropping it unended, cuts it short, so
/// that its client doesn't take it as whole. It can go to another thread, and its sends fail once
/// the session has ended.
#[derive(Debug)]
pub struct ModelResponse {
    responder: Responder,
    left: Left,
    finished: bool,
}

#[derive(Debug)]
struct Responder {
    request: u32,
    shared: Arc<Shared>,
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

    /// Why the session has closed, including a failure already seen by a request whose
    /// [`Arrival::Ended`] is still on its way to the owner.
    pub fn ended(&self) -> Option<&str> {
        self.shared.ended.get().map(String::as_str)
    }

    /// The GPUs that the sandbox's `sandboxd` saw as it started, each by its model as `nvidia-smi`
    /// names it, if it could tell ([`Hello::gpus`](schema::sandbox::Hello::gpus)).
    pub fn gpus(&self) -> Option<&[GpuModel]> {
        self.gpus.as_deref()
    }

    /// Puts [`pumpkin_subdirs()`] under [`pumpkin_dir()`], with [`dir_mode()`], which every sandbox
    /// needs before its first command.
    pub fn set_up(&mut self, left: impl Fn() -> Option<Duration>) -> Result<(), String> {
        let (root, subdirs) = (pumpkin_dir(), pumpkin_subdirs());
        let puts = subdirs.into_iter().map(|dir| Put::Dir(dir, dir_mode()));
        self.put(&root, puts, left).map_err(|error| {
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

    /// Writes `entries` below `dir` in the sandbox, creating `dir` and its missing parents. Each
    /// entry's parent must be `dir` or come earlier in `entries`, unless it exists already. Returns
    /// at the first failure, and whatever was written before it stays.
    pub fn put<'a>(
        &mut self,
        dir: &AbsolutePath,
        entries: impl IntoIterator<Item = Put<'a>>,
        left: impl Fn() -> Option<Duration>,
    ) -> io::Result<()> {
        self.shared
            .put(dir, Root::Kept, entries, Bound { left: &left })
    }

    /// Kills the sandbox's processes that [`ToSandbox::Freeze`] names, so that nothing collected
    /// afterwards changes, and returns once they are gone.
    pub fn freeze(&mut self, left: impl Fn() -> Option<Duration>) -> io::Result<()> {
        let by = Bound { left: &left };
        self.shared
            .request(|stream| ToSandbox::Freeze { stream }, by)
    }

    /// Copies the tree at the sandbox's `path`, a directory's or a file's, into `into`, a new
    /// directory, within `limits`. Fails if `into` exists or the tree passes a limit; after any
    /// failure, `into` holds whatever arrived before it.
    pub fn collect(
        &mut self,
        path: &AbsolutePath,
        into: &Path,
        limits: Limits,
        left: impl Fn() -> Option<Duration>,
    ) -> io::Result<()> {
        let by = Bound { left: &left };
        let collected = self.shared.collect(path, into, limits, by, &mut || false);
        collected.map(drop)
    }

    pub fn trees(&self) -> Trees {
        Trees {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Answers an op, promptly: the sandbox opens no other until it is. A `reply` too long for a
    /// frame refuses the op instead, and fails.
    ///
    /// # Panics
    ///
    /// Panics if `op` arrived on another session.
    pub fn reply(
        &mut self,
        op: IncomingOp,
        reply: OpReply,
        left: impl Fn() -> Option<Duration>,
    ) -> io::Result<()> {
        let by = Bound { left: &left };
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
                self.answer(number, OpReply::refused(message), by)?;
                Err(error)
            }
            answered => answered,
        }
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "taking the op, as `reply` does, answers it once"
    )]
    /// Answers an op whose payload was over the limit with its refusal.
    ///
    /// # Panics
    ///
    /// Panics if `refused` arrived on another session.
    pub fn refuse(
        &mut self,
        refused: RefusedOp,
        left: impl Fn() -> Option<Duration>,
    ) -> io::Result<()> {
        let by = Bound { left: &left };
        let message = refused.message();
        let RefusedOp {
            session,
            number,
            call: _,
            size: _,
            payload_limit: _,
        } = refused;
        self.clear_flight(session, number);
        match self.answer(number, OpReply::refused(message), by) {
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => unreachable!(
                "the refusal of op {number} is too long, though its message is \
                 `RefusedOp::message`, which names only the payload's size, a `u64`, and the \
                 limit, so its frame's message is under 200 bytes, far under \
                 `frame::MAX_MESSAGE_BYTES`, 64 KiB: {error}"
            ),
            answered => answered,
        }
    }

    fn clear_flight(&self, session: u64, number: u32) {
        // A lifetime brand tying each op to its session would reshape all of `Session`'s `pub` API.
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

    fn answer(&mut self, number: u32, reply: OpReply, by: Bound<'_>) -> io::Result<()> {
        self.shared.can_send(by, Due::Request)?;
        let message = ToSandbox::OpReply { op: number, reply };
        let writer = lock(&self.shared.writer);
        (self.shared).write(&writer, &message, &[], by, Due::Request)
    }
}

impl Shared {
    fn request(&self, message: impl FnOnce(u32) -> ToSandbox, by: Bound<'_>) -> io::Result<()> {
        let (stream, replies) = self.open(Route::Reply, by)?;
        let done = self
            .send(&message(stream), &[], by)
            .and_then(|()| self.done(&replies, by));
        self.close(stream);
        done
    }

    fn open<R>(
        &self,
        route: fn(SyncSender<R>) -> Route,
        by: Bound<'_>,
    ) -> io::Result<(u32, Receiver<R>)> {
        self.can_send(by, Due::Request)?;
        // One item in every queue. A put's, a freeze's or a start's queue holds its outcome, the
        // one reply its request takes: a second, before the first is taken, finds the queue full
        // and breaks the protocol, and one after a start's `done` reaches the running command's
        // stream, which carries only its exit. A collect's queue holds its tree's parts, and one
        // of slack lets the reader hand over a part and read on while the request writes the one
        // before: the gateway holds at most three, each with a body of at most a frame's 1 MiB,
        // and more slack would hold more memory without speeding a transfer at the slower side's
        // pace.
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

    fn send(&self, message: &ToSandbox, body: &[u8], by: Bound<'_>) -> io::Result<()> {
        let writer = lock(&self.writer);
        // A frame too long ends the session here, though `write` leaves it open: one frame of a
        // request that spans several, such as a put's, may be too long after the first was sent.
        match self.write(&writer, message, body, by, Due::Request) {
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                Err(self.write_failed(&error, Due::Request))
            }
            sent => sent,
        }
    }

    fn receive<R>(&self, replies: &Receiver<R>, by: Bound<'_>) -> io::Result<R> {
        // The gateway bounds no wait of its own. A slow link is latency, which only the operation
        // the request serves knows how long to wait out, as its deadline says; a sandbox that is
        // gone closes its connection, which ends the session at once. A deadline that has passed
        // fails at once, even with a frame waiting, so that a tree sent faster than it is written
        // ends by it too.
        loop {
            let Some(left) = by.left() else {
                return Err(self.timed_out(
                    "the sandbox didn't answer before the request's deadline: expected its \
                     answer while the deadline lasts"
                        .to_owned(),
                ));
            };
            match replies.recv_timeout(left.min(POLL)) {
                Ok(reply) => return Ok(reply),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(self.ended()),
            }
        }
    }

    fn done(&self, replies: &Receiver<Result<(), String>>, by: Bound<'_>) -> io::Result<()> {
        self.receive(replies, by)?.map_err(io::Error::other)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let reason = "the owner dropped the session".to_owned();
        // Ending the session shuts the connection both ways, which wakes the reader thread's read
        // and fails the sandbox's sends from then on.
        self.shared.end(reason);
        // Nothing else holds the reader up: its sends to the owner never block; a tree's frames
        // wait only for a collect, which has returned, dropping its queue, before anything can
        // drop the session, or which takes them on another thread, through `Trees`; and it takes
        // the routes' lock only around steps that never block, as every holder does.
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
    /// Starts the response with `status` and `headers`, and returns it, to stream its body by the
    /// deadline that `left` gives. Fails if the head is too long for a frame, which cuts the
    /// request short without a response.
    pub fn respond(
        self,
        status: u16,
        headers: Vec<(String, String)>,
        left: impl Fn() -> Option<Duration> + Send + 'static,
    ) -> io::Result<ModelResponse> {
        let Self {
            request: _,
            responder,
        } = self;
        let request = responder.request;
        let response = ModelResponse {
            responder,
            left: Left(Box::new(left)),
            finished: false,
        };
        let head = ToSandbox::ModelResponse {
            request,
            status,
            headers,
        };
        // A response that fails to start drops here, cut short.
        response.responder.send(&head, &[], response.by())?;
        Ok(response)
    }
}

impl ModelResponse {
    fn by(&self) -> Bound<'_> {
        Bound { left: &self.left.0 }
    }

    pub fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        let request = self.responder.request;
        for chunk in bytes.chunks(schema::frame::MAX_BODY_BYTES) {
            self.responder
                .send(&ToSandbox::ModelData { request }, chunk, self.by())?;
        }
        Ok(())
    }

    /// Ends the response whole.
    pub fn end(mut self) -> io::Result<()> {
        self.finish(false)
    }

    /// Ends the response cut short, which its client must not take for a whole one.
    pub fn cut(mut self) -> io::Result<()> {
        self.finish(true)
    }

    fn finish(&mut self, cut: bool) -> io::Result<()> {
        self.finished = true;
        let request = self.responder.request;
        // Out of flight before the end is sent: the sandbox may reuse the number once it reads the
        // end.
        lock(&self.responder.shared.routes).models.remove(&request);
        self.responder
            .send(&ToSandbox::ModelEnd { request, cut }, &[], self.by())
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
    fn send(&self, message: &ToSandbox, body: &[u8], by: Bound<'_>) -> io::Result<()> {
        let writer = lock(&self.shared.writer);
        // Checked with the lock held, as waiting for the lock may use up the deadline.
        self.shared.can_send(by, Due::Response)?;
        (self.shared).write(&writer, message, body, by, Due::Response)
    }
}

// Whose deadline a frame is due by, which the reason for the session's end names if it passes: a
// request's, which the owner's reply to an op shares, or a response's.
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
    fn can_send(&self, by: Bound<'_>, due: Due) -> io::Result<()> {
        if self.ended.get().is_some() {
            return Err(self.ended());
        }
        if by.left().is_none() {
            let reason = format!(
                "the {}'s deadline passed before it was sent: expected time left to send it",
                due.noun()
            );
            return Err(self.timed_out(reason));
        }
        Ok(())
    }

    fn timed_out(&self, reason: String) -> io::Error {
        self.end(reason.clone());
        io::Error::new(io::ErrorKind::TimedOut, reason)
    }

    fn write(
        &self,
        _writer: &MutexGuard<'_, ()>,
        message: &ToSandbox,
        body: &[u8],
        by: Bound<'_>,
        due: Due,
    ) -> io::Result<()> {
        let connection = &self.connection;
        match schema::frame::write(&mut Until { connection, by }, message, body) {
            // A frame too long, which `frame::write` refuses before writing anything, leaves the
            // session open, so that a reply too long can still be answered with a refusal; any
            // other failure ends it.
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => Err(error),
            written => written.map_err(|error| self.write_failed(&error, due)),
        }
    }

    fn write_failed(&self, error: &io::Error, due: Due) -> io::Error {
        // A failed write may leave part of a frame on the connection.
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

    fn end(&self, reason: String) -> &str {
        let mut routes = lock(&self.routes);
        // Closed first, under the lock, so that no write reaches the sandbox once anything can
        // learn that the session ended: an owner woken by the end may still answer an op. A
        // connection already closed is fine. The reason is set under the lock too, so that no
        // route opens after the routes are cleared. A reason already set is the session's.
        let _ = self.connection.shutdown();
        let recorded = self.ended.get_or_init(|| reason);
        routes.open.clear();
        recorded
    }

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

fn route(mut ops: Assembler) {
    let shared = Arc::clone(&ops.shared);
    let routes = &shared.routes;
    let mut reader = BufReader::new(&shared.connection);
    let reason = loop {
        let (message, body) = match schema::frame::read::<FromSandbox>(&mut reader) {
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

struct Owner {
    session: u64,
    send: Box<dyn Fn(Arrival) + Send>,
}

impl Owner {
    fn arrived(&self, arrival: Arrival) {
        (self.send)(arrival);
    }
}

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
        // A request closes its stream as it returns, even if it gave up before `sandboxd` was done
        // answering, as a collect does at a limit its tree passes, or any request at its deadline.
        // `sandboxd` can't know, and may still be sending what it had begun, so a reply on a closed
        // stream is an expected race, not a break.
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
            // The send waits for the request, which writes the tree as it reads it and needs the
            // lock to close its stream, so it runs with the lock released; a failed request drops
            // the queue, which ends the wait.
            let sender = sender.clone();
            drop(locked);
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

struct Assembler {
    owner: Owner,
    // The most bytes an op's payload may hold, if the session takes ops.
    limit: Option<OpPayloadLimit>,
    open: Option<(u32, Assembly)>,
    takes_models: bool,
    shared: Arc<Shared>,
    // The model requests whose bodies are arriving, by number, each with the bytes still due.
    models: BTreeMap<u32, (ModelRequest, u64)>,
}

struct Assembly {
    call: OpCall,
    // The payload's bytes still to arrive: the op is whole once none are.
    due: u64,
    payload: Payload,
}

enum Payload {
    Kept(Vec<u8>),
    // The limit is the session's, not the protocol's, so a payload past it is no break: its bytes
    // arrive and are dropped, and the op is refused only once all have, so that the sandbox's next
    // op follows it whole.
    Dropped { size: u64, limit: OpPayloadLimit },
}

impl Assembler {
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
                op: op::whole(call, payload),
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
        // `ModelRequest`'s readers rely on lowercase names, as the protocol has them. The message
        // leaves the name unquoted, as it may be anything the sandbox chose.
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
        // The session's model requests in its owner's inbox are all in flight, so this bounds how
        // many it leaves there, as `Registration::accept` counts on.
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

    fn pass_on_model(&self, number: u32, request: ModelRequest) {
        let responder = Responder {
            request: number,
            shared: Arc::clone(&self.shared),
        };
        let incoming = IncomingModelRequest { request, responder };
        self.owner.arrived(Arrival::ModelRequest(incoming));
    }
}

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

const SANDBOX_ENDED: &str = "the sandbox ended the session";

fn closed_by_sandbox(error: &io::Error) -> bool {
    // A sandbox that closes its end with what the gateway sent still unread resets the connection
    // rather than ends it cleanly, which a read then fails with, and a write once the reset has
    // come, as a broken pipe; which of these the gateway meets first is a race.
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
    )
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    stdx::unpoisoned(mutex.lock())
}

fn context(what: impl fmt::Display, error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{what}: {error}"))
}

fn broke(what: &str) -> String {
    format!("the sandbox broke the protocol: {what}")
}

fn out_of_turn(stream: u32, reply: &Reply, expected: &str) -> String {
    broke(&format!(
        "it sent `{}` on stream {stream} out of turn: expected {expected}",
        reply.name()
    ))
}
