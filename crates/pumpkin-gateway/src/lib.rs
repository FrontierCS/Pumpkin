//! The gateway's end of the sessions that sandboxes open; the protocol is
//! [`pumpkin_schema::sandbox`].
//!
//! Each sandbox gets a [`Listener`] of its own: a Unix socket that only that sandbox can
//! reach. The first connection on it becomes the sandbox's [`Session`] and the socket is
//! removed, so no second connection can follow. The socket, not a secret, tells which sandbox
//! is talking.
//!
//! Everything a session receives is untrusted, since the sandbox may run an agent's code. A
//! message that fails to parse or comes out of turn, more replies than a waiting request takes,
//! silence for [`REPLY_TIMEOUT`] while a request waits, or a write the sandbox doesn't read
//! within it ends the session, which stops the sandbox. A collected tree is written into a
//! new directory of its own, within [`Limits`].
//!
//! # Ops
//!
//! A sandbox whose session takes ops ([`Listener::accept`]) may make them, as
//! [the protocol](pumpkin_schema::sandbox) describes: the owner takes each with
//! [`Session::next_op`] and answers it with [`Session::reply`]. The gateway authorizes nothing,
//! but bounds what ops cost the host, and ends a session whose sandbox breaks the ops' rules.

use std::collections::HashMap;
use std::io::{self, BufReader};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};
use std::{fmt, fs, mem};

use pumpkin_schema::ir::{AbsolutePath, Name};
use pumpkin_schema::sandbox::op::{OpCall, OpReply};
use pumpkin_schema::sandbox::tree::{self, Limits};
use pumpkin_schema::sandbox::{
    Entry, ExitStatus, FromSandbox, Launch, MAX_OP_PAYLOAD_BYTES, OpPayloadLimit, RelativePath,
    ToSandbox, frame,
};

/// How long a session waits on the sandbox: for its next frame, and for it to read a write.
///
/// `sandboxd` answers every request promptly, reads what it is sent, and sends a tree's frames
/// one after another, so a sandbox that stalls this long is stuck or hostile. The limit holds
/// for each frame and each write, not for a whole request: a tree's transfer is bounded only
/// by its [`Limits`] times this.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// How many frames of a tree may wait for the request writing it. While that many wait, the
/// session reads nothing more.
// One frame of slack. Reading the socket and writing to disk overlap without any, as the reader
// reads a frame while the request writes the one before; the slack lets the reader hand over
// the frame it has read and go on reading before that write ends. The gateway then holds at
// most three frames of a tree, one being written, one waiting and one being read, each with up
// to a frame's 1 MiB body; more would hold more without speeding the transfer, which goes at
// the pace of the slower side.
const QUEUED_TREE_FRAMES: usize = 1;

/// How many frames may wait for any other request: its reply and, for a command, its exit.
const QUEUED_REPLIES: usize = 2;

/// The number of the next session the process makes, which tells it apart from every other, so
/// that an op is answered only on the session it arrived on.
// Shared by every session the process makes, on whatever thread makes it.
static NEXT_SESSION: AtomicU64 = AtomicU64::new(0);

/// The socket one sandbox connects to.
#[derive(Debug)]
pub struct Listener {
    listener: UnixListener,
    path: PathBuf,
}

impl Listener {
    /// Listens at `path`. Any user may connect, since a sandbox's user is not the gateway's;
    /// only the sandbox can reach the socket's directory.
    ///
    /// # Errors
    ///
    /// Fails if the socket can't be created.
    pub fn bind(path: PathBuf) -> io::Result<Self> {
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666))?;
        Ok(Self { listener, path })
    }

    /// Waits up to `timeout` for the sandbox to connect, then removes the socket. The session
    /// takes ops if `ops` gives the most bytes an op's payload may hold: it holds one payload at
    /// most, and refuses a payload over the limit ([`Arrival::Refused`]), keeping none of its
    /// bytes. With `None`, an op breaks the protocol.
    ///
    /// # Errors
    ///
    /// Fails with [`io::ErrorKind::TimedOut`] if nothing connects in time, or if accepting the
    /// connection or setting up its session fails.
    pub fn accept(self, timeout: Duration, ops: Option<OpPayloadLimit>) -> io::Result<Session> {
        let path = self.path.display();
        self.listener
            .set_nonblocking(true)
            .map_err(|error| context(format_args!("{path} can't be made non-blocking"), &error))?;
        let deadline = Instant::now() + timeout;
        let stream = loop {
            match self.listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("no sandbox connected to {path} within {timeout:?}"),
                        ));
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => {
                    let what = format_args!("a connection to {path} can't be accepted");
                    return Err(context(what, &error));
                }
            }
        };
        stream
            .set_nonblocking(false)
            .map_err(|error| context("the sandbox's connection can't be made blocking", &error))?;
        Session::new(stream, ops)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Removing the socket keeps a second connection out; a socket already gone is fine.
        let _ = fs::remove_file(&self.path);
    }
}

/// A sandbox's session, which one owner drives: it alone writes to the sandbox.
///
/// Requests on it block until `sandboxd` answers or the session ends. Dropping it ends the
/// session, which stops the sandbox.
#[derive(Debug)]
pub struct Session {
    /// The session's number, from [`NEXT_SESSION`], which its ops carry.
    id: u64,
    connection: UnixStream,
    // Shared with the reader thread, which routes the sandbox's frames by them, and removes a
    // route once its command has exited or nothing receives from it.
    routes: Arc<Mutex<Routes>>,
    // Shared with the reader thread, which may end the session, and with each `Process`, which
    // reports why it ended. Set under the routes' lock, which `end` explains.
    ended: Arc<OnceLock<String>>,
    next_stream: u32,
    /// The op that has arrived whole, or that the gateway refuses, if any, from the reader
    /// thread.
    ops: Receiver<Arrival>,
}

/// Where frames from the sandbox go: the requests waiting for them, by stream, and whether the
/// sandbox may open an op.
#[derive(Debug)]
struct Routes {
    open: HashMap<u32, Route>,
    /// The op in flight, by number: opened, and not yet answered.
    op: Option<u32>,
}

/// The way to a request waiting for frames.
#[derive(Clone, Debug)]
struct Route {
    // The reader thread sends the request's frames, which the session's owner receives.
    sender: SyncSender<Reply>,
    queue: Queue,
}

/// What a request waits for.
#[derive(Clone, Copy, Debug)]
enum Queue {
    /// A reply and, for a command, its exit: a frame that finds the queue full breaks the
    /// protocol.
    Replies,
    /// A tree, whose frames wait for the request to write them.
    Tree,
}

type Reply = (FromSandbox, Vec<u8>);

/// What to put in a sandbox: a directory, or a file with its bytes.
#[derive(Clone, Copy, Debug)]
pub enum Put<'a> {
    Dir(&'a RelativePath),
    File(&'a RelativePath, &'a [u8]),
}

/// An op a caller in the sandbox made, whole, and in flight until [`Session::reply`] answers
/// it, on the session that handed it out.
#[derive(Debug)]
#[must_use = "an op dropped unanswered wedges the sandbox's ops, which wait on it for good: \
              answer it with `Session::reply`"]
pub struct IncomingOp {
    /// The number of the session the op arrived on.
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
}

/// What arrived for the session's owner, as [`Session::next_op`] hands it out: an op to answer,
/// or word of one that the gateway refused itself.
#[derive(Debug)]
#[must_use = "an arrival may hold an op, and an op dropped unanswered wedges the sandbox's ops: \
              answer each with `Session::reply`"]
pub enum Arrival {
    /// An op that arrived whole, for the owner to answer.
    Op(IncomingOp),
    /// An op whose payload was over the session's limit. The gateway refused it, and sent the
    /// refusal unless the session had ended, so the owner answers nothing, but learns that the
    /// op was made.
    Refused(RefusedOp),
}

/// An op that the gateway refused, since its payload was over the session's limit, and whose
/// refusal it sent unless the session had ended: what a record of the op needs.
#[derive(Debug)]
pub struct RefusedOp {
    /// The sandbox's number for the op, which the refusal names.
    number: u32,
    /// What the op asked: a call that takes a payload, an [`OpCall::Judge`] or an
    /// [`OpCall::Submit`].
    pub call: OpCall,
    /// The payload's size, in bytes, which all arrived and was dropped.
    pub size: u64,
    /// The limit the payload was over, which the session was accepted with.
    pub payload_limit: OpPayloadLimit,
    /// Why the op was refused: the message its refusal carries.
    pub message: String,
}

/// A command started in a sandbox.
#[derive(Debug)]
pub struct Process {
    replies: Receiver<Reply>,
    // Shared with the session and its reader thread, which record why the session ended.
    ended: Arc<OnceLock<String>>,
    exited: Option<ExitStatus>,
}

impl Process {
    /// Waits up to `timeout` for the command to exit: `None` if it is still running.
    ///
    /// # Errors
    ///
    /// Fails if the session ends first.
    pub fn wait(&mut self, timeout: Duration) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.exited {
            return Ok(Some(status));
        }
        match self.replies.recv_timeout(timeout) {
            Ok((FromSandbox::Exited { status, .. }, _)) => {
                self.exited = Some(status);
                Ok(Some(status))
            }
            Ok((other, _)) => Err(unexpected(&other)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(ended(&self.ended)),
        }
    }
}

impl Session {
    fn new(stream: UnixStream, limit: Option<OpPayloadLimit>) -> io::Result<Self> {
        // A socket option, so it holds for every handle on the connection.
        stream
            .set_write_timeout(Some(REPLY_TIMEOUT))
            .map_err(|error| context("the connection's write timeout can't be set", &error))?;
        let routes = Arc::new(Mutex::new(Routes {
            open: HashMap::new(),
            op: None,
        }));
        let ended = Arc::new(OnceLock::new());
        // From the reader thread to the owner. Every arrival is the op in flight, so the queue
        // never holds more than one.
        let (arrivals, ops) = mpsc::sync_channel(1);
        let reader = stream
            .try_clone()
            .map_err(|error| context("the connection can't be cloned for its reader", &error))?;
        let (reader_routes, reader_ended) = (Arc::clone(&routes), Arc::clone(&ended));
        let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
        let assembler = Assembler {
            session: id,
            arrivals,
            limit,
            open: None,
        };
        // Reads the sandbox's frames while the owner writes requests and waits on their replies.
        thread::Builder::new()
            .name("session".to_owned())
            .spawn(move || route(&reader, &reader_routes, &reader_ended, assembler))
            .map_err(|error| context("the session's reader thread didn't start", &error))?;
        Ok(Self {
            id,
            connection: stream,
            routes,
            ended,
            next_stream: 1,
            ops,
        })
    }

    /// Writes `entries` below `dir` in the sandbox, creating `dir` and its missing parents.
    /// Each entry's parent is `dir` or comes earlier in `entries`, unless it exists already.
    ///
    /// # Errors
    ///
    /// Fails if `sandboxd` can't write them or the session ends.
    pub fn put(&mut self, dir: &AbsolutePath, entries: &[Put<'_>]) -> io::Result<()> {
        let (stream, replies) = self.open(Queue::Replies)?;
        let put = self
            .send_tree(stream, dir, entries)
            .and_then(|()| self.done(&replies));
        self.close(stream);
        put
    }

    /// Starts a command.
    ///
    /// # Errors
    ///
    /// Fails if the command doesn't start or the session ends.
    pub fn start(&mut self, launch: Launch) -> io::Result<Process> {
        let (stream, replies) = self.open(Queue::Replies)?;
        let started = self
            .send(&ToSandbox::Start { stream, launch }, &[])
            .and_then(|()| self.done(&replies));
        match started {
            // The reader removes the command's route, which outlives this call.
            Ok(()) => Ok(Process {
                replies,
                ended: Arc::clone(&self.ended),
                exited: None,
            }),
            Err(error) => {
                self.close(stream);
                Err(error)
            }
        }
    }

    /// Kills every process in the sandbox but `sandboxd`, so that nothing collected afterwards
    /// changes, and returns once they are gone.
    ///
    /// # Errors
    ///
    /// Fails if `sandboxd` can't kill them or the session ends.
    pub fn freeze(&mut self) -> io::Result<()> {
        self.request(|stream| ToSandbox::Freeze { stream })
    }

    /// Copies the tree in the sandbox's `dir` into `into`, a new directory, within `limits`.
    /// After a failure, `into` holds whatever arrived before it.
    ///
    /// # Errors
    ///
    /// Fails if `into` exists, if `sandboxd` can't send the tree, if the tree breaks the
    /// protocol or passes a limit, or if the session ends.
    pub fn collect(&mut self, dir: &AbsolutePath, into: &Path, limits: Limits) -> io::Result<()> {
        let (stream, replies) = self.open(Queue::Tree)?;
        let created = fs::create_dir(into).map_err(|error| context(into.display(), &error));
        let collected = created.and_then(|()| {
            let tree = tree::Writer::new(into.to_owned(), limits);
            self.send(
                &ToSandbox::Collect {
                    stream,
                    dir: dir.clone(),
                },
                &[],
            )?;
            self.receive_tree(&replies, tree)
        });
        self.close(stream);
        collected
    }

    /// Waits up to `timeout` for the next op whose payload has all arrived: `None` if none did.
    /// Answer each promptly, as [the protocol](pumpkin_schema::sandbox) requires. A refused
    /// op's refusal is sent here, which may wait on a write for up to [`REPLY_TIMEOUT`].
    ///
    /// # Errors
    ///
    /// Fails once the session has ended and every op that arrived whole before has been handed
    /// out.
    pub fn next_op(&mut self, timeout: Duration) -> io::Result<Option<Arrival>> {
        let arrival = match self.ops.recv_timeout(timeout) {
            Ok(arrival) => arrival,
            Err(RecvTimeoutError::Timeout) => return Ok(None),
            Err(RecvTimeoutError::Disconnected) => return Err(ended(&self.ended)),
        };
        match &arrival {
            Arrival::Op(_) => {}
            Arrival::Refused(refused) => {
                self.clear_flight();
                let message = refused.message.clone();
                match self.answer(refused.number, OpReply::Refused { message }) {
                    #[expect(
                        clippy::unreachable,
                        reason = "the refusal's message is the one `Assembler::pass_on` writes, \
                                  which names only the payload's size and the limit, two u64s, \
                                  so its frame's message is a few hundred bytes, far under \
                                  `frame::MAX_MESSAGE_BYTES`, 64 KiB"
                    )]
                    Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                        unreachable!("the refusal of op {} is too long: {error}", refused.number)
                    }
                    // Any other failed write has ended the session, which the next call
                    // reports; the op is handed out all the same.
                    Ok(()) | Err(_) => {}
                }
            }
        }
        Ok(Some(arrival))
    }

    /// Answers an op, which lets the sandbox send its next. A reply too long for a frame reaches
    /// the caller as a refusal that says so.
    ///
    /// # Errors
    ///
    /// Fails if the reply is too long for a frame, or the session has ended.
    ///
    /// # Panics
    ///
    /// Panics if `op` arrived on another session.
    pub fn reply(&mut self, op: IncomingOp, reply: OpReply) -> io::Result<()> {
        let IncomingOp {
            session,
            number,
            op,
        } = op;
        #[expect(
            clippy::unreachable,
            reason = "the owner answers each op on the session that handed it out, as this \
                      method's contract requires: on another, the op's number would name an op \
                      that sandbox may not have made"
        )]
        if session != self.id {
            unreachable!("op {number} was answered on a session other than the one it arrived on")
        }
        // The op leaves the flight once, before its answer is written: the sandbox may open the
        // next as soon as it reads an answer, which a second clearing would drop. Its payload
        // goes first, so that the next op's buffer is never held beside it while the write waits.
        drop(op);
        self.clear_flight();
        match self.answer(number, reply) {
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                let message = format!("the reply was too long to send: {error}");
                self.answer(number, OpReply::Refused { message })?;
                Err(error)
            }
            answered => answered,
        }
    }

    fn clear_flight(&self) {
        lock(&self.routes).op = None;
    }

    /// Sends `reply` to op `number`, already out of the flight.
    fn answer(&mut self, number: u32, reply: OpReply) -> io::Result<()> {
        let message = ToSandbox::OpReply { op: number, reply };
        match frame::write(&mut self.connection, &message, &[]) {
            // A reply too long for a frame, which `frame::write` refuses before writing
            // anything, so the session goes on, and the op can still be answered.
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => Err(error),
            written => written.map_err(|error| self.write_failed(&error)),
        }
    }

    fn send_tree(
        &mut self,
        stream: u32,
        dir: &AbsolutePath,
        entries: &[Put<'_>],
    ) -> io::Result<()> {
        self.send(
            &ToSandbox::Put {
                stream,
                dir: dir.clone(),
            },
            &[],
        )?;
        for entry in entries {
            match *entry {
                Put::Dir(path) => {
                    let entry = Entry::Dir { path: path.clone() };
                    self.send(&ToSandbox::Entry { stream, entry }, &[])?;
                }
                Put::File(path, bytes) => {
                    let size = u64::try_from(bytes.len()).map_err(io::Error::other)?;
                    let entry = Entry::File {
                        path: path.clone(),
                        size,
                    };
                    self.send(&ToSandbox::Entry { stream, entry }, &[])?;
                    for chunk in bytes.chunks(frame::MAX_BODY_BYTES) {
                        self.send(&ToSandbox::Data { stream }, chunk)?;
                    }
                }
            }
        }
        self.send(&ToSandbox::End { stream }, &[])
    }

    fn receive_tree(&self, replies: &Receiver<Reply>, mut tree: tree::Writer) -> io::Result<()> {
        loop {
            match self.receive(replies)? {
                (FromSandbox::Entry { entry, .. }, _) => tree.entry(&entry)?,
                (FromSandbox::Data { .. }, body) => tree.data(&body)?,
                (FromSandbox::End { .. }, _) => return tree.finish(),
                (FromSandbox::Failed { message, .. }, _) => return Err(io::Error::other(message)),
                // The reader passes op frames to the assembler, never to a request.
                (
                    other @ (FromSandbox::Done { .. }
                    | FromSandbox::Exited { .. }
                    | FromSandbox::Op { .. }
                    | FromSandbox::OpData { .. }),
                    _,
                ) => return Err(unexpected(&other)),
            }
        }
    }

    /// Sends the request `message` builds for a new stream, and waits for its reply.
    fn request(&mut self, message: impl FnOnce(u32) -> ToSandbox) -> io::Result<()> {
        let (stream, replies) = self.open(Queue::Replies)?;
        let done = self
            .send(&message(stream), &[])
            .and_then(|()| self.done(&replies));
        self.close(stream);
        done
    }

    fn open(&mut self, queue: Queue) -> io::Result<(u32, Receiver<Reply>)> {
        let stream = self.next_stream;
        self.next_stream = stream.wrapping_add(1);
        let (sender, replies) = mpsc::sync_channel(match queue {
            Queue::Replies => QUEUED_REPLIES,
            Queue::Tree => QUEUED_TREE_FRAMES,
        });
        let mut routes = lock(&self.routes);
        if self.ended.get().is_some() {
            return Err(ended(&self.ended));
        }
        routes.open.insert(stream, Route { sender, queue });
        Ok((stream, replies))
    }

    fn close(&self, stream: u32) {
        lock(&self.routes).open.remove(&stream);
    }

    /// Sends a frame. Any failure ends the session: one frame of a request that spans several,
    /// such as a [`put`](Self::put)'s, may be too long after the first was sent.
    fn send(&mut self, message: &ToSandbox, body: &[u8]) -> io::Result<()> {
        frame::write(&mut self.connection, message, body).map_err(|error| self.write_failed(&error))
    }

    /// Ends the session after a write that failed with `error`. A write that fails, or that the
    /// sandbox doesn't read within [`REPLY_TIMEOUT`], may leave part of a frame on the
    /// connection. The session's reason, which the error returned gives, is the failed write's,
    /// unless it had ended already.
    fn write_failed(&self, error: &io::Error) -> io::Error {
        let reason = if matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ) {
            format!("the sandbox read nothing for {REPLY_TIMEOUT:?}")
        } else {
            format!("writing to the sandbox failed: {error}")
        };
        end(&self.routes, &self.ended, &self.connection, reason);
        ended(&self.ended)
    }

    fn receive(&self, replies: &Receiver<Reply>) -> io::Result<Reply> {
        match replies.recv_timeout(REPLY_TIMEOUT) {
            Ok(reply) => Ok(reply),
            Err(RecvTimeoutError::Timeout) => {
                let silent = format!("the sandbox sent nothing for {REPLY_TIMEOUT:?}");
                end(&self.routes, &self.ended, &self.connection, silent);
                Err(ended(&self.ended))
            }
            Err(RecvTimeoutError::Disconnected) => Err(ended(&self.ended)),
        }
    }

    fn done(&self, replies: &Receiver<Reply>) -> io::Result<()> {
        match self.receive(replies)? {
            (FromSandbox::Done { .. }, _) => Ok(()),
            (FromSandbox::Failed { message, .. }, _) => Err(io::Error::other(message)),
            (
                other @ (FromSandbox::Exited { .. }
                | FromSandbox::Entry { .. }
                | FromSandbox::Data { .. }
                | FromSandbox::End { .. }
                | FromSandbox::Op { .. }
                | FromSandbox::OpData { .. }),
                _,
            ) => Err(unexpected(&other)),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Closing the connection ends the session; the reader thread then sees it close.
        let _ = self.connection.shutdown(Shutdown::Both);
    }
}

/// Reads frames from the sandbox until the session ends: each reply goes to the request
/// waiting on its stream, and each op's frames to `ops`.
fn route(
    connection: &UnixStream,
    routes: &Mutex<Routes>,
    ended: &OnceLock<String>,
    mut ops: Assembler,
) {
    // Dropped before `ops`, a parameter, whose sender to the owner closes as `ops` drops: the
    // owner, seeing it closed, asks why the session ended, which must be known by then.
    let mut ending = Ending {
        routes,
        ended,
        connection,
        reason: "the session's reader thread panicked".to_owned(),
    };
    let mut reader = BufReader::new(connection);
    ending.reason = loop {
        let (message, body) = match frame::read::<FromSandbox>(&mut reader) {
            Ok(Some(frame)) => frame,
            Ok(None) => break "the sandbox ended the session".to_owned(),
            Err(error) => break format!("the session failed: {error}"),
        };
        let outcome = match message {
            FromSandbox::Op { op, call, size } => ops.open(routes, op, call, size),
            FromSandbox::OpData { op } => ops.data(op, &body),
            reply @ (FromSandbox::Done { stream }
            | FromSandbox::Failed { stream, .. }
            | FromSandbox::Exited { stream, .. }
            | FromSandbox::Entry { stream, .. }
            | FromSandbox::Data { stream }
            | FromSandbox::End { stream }) => route_reply(routes, stream, (reply, body)),
        };
        if let Err(reason) = outcome {
            break reason;
        }
    };
}

/// Ends the session for `reason` as it drops. The reader thread holds one, so that the session
/// ends however the thread leaves, unwinding from a panic included.
struct Ending<'a> {
    routes: &'a Mutex<Routes>,
    ended: &'a OnceLock<String>,
    connection: &'a UnixStream,
    reason: String,
}

impl Drop for Ending<'_> {
    fn drop(&mut self) {
        let reason = mem::take(&mut self.reason);
        end(self.routes, self.ended, self.connection, reason);
    }
}

/// Hands a reply to the request waiting on `stream`. A reply for no waiting request is
/// dropped: its request has given up. Removes the route after a command's exit (its stream's
/// last frame) or once its receiver is gone.
fn route_reply(routes: &Mutex<Routes>, stream: u32, reply: Reply) -> Result<(), String> {
    let Some(route) = lock(routes).open.get(&stream).cloned() else {
        return Ok(());
    };
    let exited = matches!(reply.0, FromSandbox::Exited { .. });
    let given_up = match route.queue {
        // The request writes the tree as it reads it, or drops the queue when it fails.
        Queue::Tree => route.sender.send(reply).is_err(),
        Queue::Replies => match route.sender.try_send(reply) {
            Ok(()) => false,
            Err(TrySendError::Disconnected(_)) => true,
            Err(TrySendError::Full(_)) => {
                return Err(broke(&format!(
                    "it sent more on stream {stream} than a request takes"
                )));
            }
        },
    };
    // A command's `Process` outlives the call that started it, so no call closes its route.
    if exited || given_up {
        lock(routes).open.remove(&stream);
    }
    Ok(())
}

/// Assembles the op the sandbox sends, on the reader thread, and passes it on whole.
struct Assembler {
    /// The number of the session the ops arrive on.
    session: u64,
    // To the session's owner, which takes each op with `Session::next_op`.
    arrivals: SyncSender<Arrival>,
    /// The most bytes an op's payload may hold, if the session takes ops.
    limit: Option<OpPayloadLimit>,
    /// The op whose payload is arriving, by number.
    open: Option<(u32, Assembly)>,
}

/// An op whose payload is arriving.
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
    /// Puts op `number`, whose payload is `size` bytes, in flight in `routes`, if the session
    /// takes ops and none is in flight, and passes it on at once if it has no payload.
    fn open(
        &mut self,
        routes: &Mutex<Routes>,
        number: u32,
        call: OpCall,
        size: u64,
    ) -> Result<(), String> {
        let Some(limit) = self.limit else {
            return Err(broke(&format!(
                "it opened op {number}, but the session takes no ops"
            )));
        };
        let mut routes = lock(routes);
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
        #[expect(
            clippy::unreachable,
            reason = "a frame's body holds at most `frame::MAX_BODY_BYTES`, 1 MiB, which fits a \
                      u64"
        )]
        let Ok(len) = u64::try_from(bytes.len()) else {
            unreachable!("a body of {} bytes overflows a u64", bytes.len())
        };
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
        let arrival = match payload {
            Payload::Kept(payload) => Arrival::Op(IncomingOp {
                session: self.session,
                number,
                op: whole(call, payload),
            }),
            Payload::Dropped { size, limit } => Arrival::Refused(RefusedOp {
                number,
                call,
                size,
                payload_limit: limit,
                message: format!(
                    "the payload is {size} bytes, more than the limit of {0}: send at most {0}",
                    limit.bytes()
                ),
            }),
        };
        match self.arrivals.try_send(arrival) {
            // The owner has dropped the session, so nobody is left to answer.
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            #[expect(
                clippy::unreachable,
                reason = "the queue holds one arrival, and holds none now: an op leaves the \
                          flight only once the owner has taken its arrival from the queue, as \
                          it answers an op it took, or `next_op` refuses one it took, and the \
                          sandbox may open the next op only once the one before has left"
            )]
            Err(TrySendError::Full(_)) => {
                unreachable!("op {number} arrived while the queue held another op's arrival")
            }
        }
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
    let takes_payload = match call {
        OpCall::Judge { .. } | OpCall::Submit { .. } => true,
        OpCall::Ops | OpCall::Status { .. } => false,
    };
    if size > 0 && !takes_payload {
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
    #[expect(
        clippy::unreachable,
        reason = "the size is at most the limit, which is at most `MAX_OP_PAYLOAD_BYTES`, \
                  64 MiB (an `OpPayloadLimit` holds no more), which fits the usize of every \
                  Linux target, of 32 bits or more"
    )]
    let Ok(capacity) = usize::try_from(size) else {
        unreachable!("a payload of {size} bytes overflows a usize")
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
    }
}

/// Ends the session, unless it has ended already: closes the connection, which stops the
/// sandbox, records why in `ended`, and wakes every request waiting on it.
fn end(routes: &Mutex<Routes>, ended: &OnceLock<String>, connection: &UnixStream, reason: String) {
    let mut routes = lock(routes);
    // Closed first, under the lock, so that no write reaches the sandbox once anything can
    // learn the session ended: an owner woken by the end may still answer an op. A connection
    // already closed is fine. The reason is set under the lock too, so that no route opens
    // after the routes are cleared. A reason already set is the session's.
    let _ = connection.shutdown(Shutdown::Both);
    let _ = ended.set(reason);
    routes.open.clear();
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The error of a request on a session that has ended, for the reason in `ended`.
fn ended(ended: &OnceLock<String>) -> io::Error {
    #[expect(
        clippy::unreachable,
        reason = "every way to learn that the session ended follows the reason's recording: \
                  `end` records it before it closes the routes, which close otherwise only \
                  after a frame that ends their request or once nothing receives from them; \
                  the ops' sender closes only after the reader thread's `Ending` has ended the \
                  session, even as a panic unwinds it; and the rest call `end` or check the \
                  reason first"
    )]
    let Some(reason) = ended.get() else {
        unreachable!("the session ended without a reason")
    };
    io::Error::new(io::ErrorKind::ConnectionAborted, reason.clone())
}

fn unexpected(message: &FromSandbox) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        broke(&format!("it sent {message:?} out of turn")),
    )
}

/// `error`, prefixed with what failed, as `<what>: <error>`, keeping its kind.
fn context(what: impl fmt::Display, error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{what}: {error}"))
}

/// Why a session ends when the sandbox does `what`.
fn broke(what: &str) -> String {
    format!("the sandbox broke the protocol: {what}")
}
