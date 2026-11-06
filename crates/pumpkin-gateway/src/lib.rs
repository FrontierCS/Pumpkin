//! The gateway's end of the sessions that sandboxes open; the protocol is
//! [`pumpkin_schema::sandbox`].
//!
//! Each sandbox gets a [`Listener`] of its own: a Unix socket that only that sandbox can
//! reach. The first connection on it becomes the sandbox's [`Session`] and the socket is
//! removed, so no second connection can follow. The socket, not a secret, tells which sandbox
//! is talking.
//!
//! A sandbox, however buggy, can't harm the host through its session. A message that fails to
//! parse or comes out of turn, more replies than a waiting request takes, or a malformed tree
//! ends the session, which stops the sandbox. A late reply, on a stream whose request has
//! returned, is dropped, since a request that gives up closes its stream while `sandboxd` may
//! still be answering. A collected tree is written into a new directory of its own, within
//! [`Limits`]: a tree that passes them fails only its collect.
//!
//! # Deadlines
//!
//! Every request waits on the sandbox until the deadline its caller gives, so a slow link makes
//! it wait, never fail. A request whose deadline passes fails with [`io::ErrorKind::TimedOut`]
//! and ends the session. A sandbox that closes its connection ends the session at once.
//!
//! # Ops
//!
//! A sandbox whose session takes ops ([`Listener::accept`]) may make them, as
//! [the protocol](pumpkin_schema::sandbox) describes: each reaches the owner as an [`Arrival`],
//! which it answers with [`Session::reply`], or [`Session::refuse`] if the payload was over the
//! session's limit. The gateway authorizes nothing, but bounds what ops cost the host, and ends a
//! session whose sandbox breaks the ops' rules.

use std::collections::BTreeMap;
use std::io::{self, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::{fmt, fs, mem, process};

use pumpkin_schema::ir::{AbsolutePath, Name};
use pumpkin_schema::sandbox::op::{OpCall, OpReply};
use pumpkin_schema::sandbox::tree::{self, Limits};
use pumpkin_schema::sandbox::{
    Entry, ExitStatus, FromSandbox, Launch, MAX_OP_PAYLOAD_BYTES, OpPayloadLimit, RelativePath,
    ToSandbox, frame, pumpkin_dir, pumpkin_subdirs,
};
use stdx::Deadline;

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

    /// Waits until `by` for the sandbox to connect, then removes the socket. The session's
    /// reader thread sends each [`Arrival`] on `arrivals` as `wrap` makes it; `wrap` runs on the
    /// reader, which it holds up, so it only wraps.
    /// Only [`Arrival::Exited`] names its session, so give each live session a receiver of its
    /// own; one may serve the next session once this one has dropped ([`Session`]).
    /// The session takes ops if `ops` gives the most bytes an op's payload may hold: it holds
    /// one payload at most, and keeps none of the bytes of a payload over the limit
    /// ([`Arrival::Refused`]). With `None`, an op breaks the protocol.
    ///
    /// # Errors
    ///
    /// Fails with [`io::ErrorKind::TimedOut`] if nothing connects by `by`, or if accepting the
    /// connection or setting up its session fails.
    pub fn accept<T: Send + 'static>(
        self,
        by: Deadline,
        ops: Option<OpPayloadLimit>,
        arrivals: Sender<T>,
        wrap: fn(Arrival) -> T,
    ) -> io::Result<Session> {
        let path = self.path.display();
        self.listener
            .set_nonblocking(true)
            .map_err(|error| context(format_args!("{path} can't be made non-blocking"), &error))?;
        let stream = loop {
            match self.listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if by.remaining().is_none() {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!(
                                "no sandbox connected to {path} by the deadline: expected its \
                                 sandbox to connect while the deadline lasts"
                            ),
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
        // Unbounded, so the reader never waits on its owner; the protocol bounds what waits
        // there: the op in flight, the exit of each command the owner started, and the end. An
        // owner that has dropped its receiver hears nothing more, as it chose.
        let send = move |arrival| {
            let _ = arrivals.send(wrap(arrival));
        };
        Session::new(stream, ops, Box::new(send))
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
/// Requests on it block until `sandboxd` answers, their deadline passes, or the session ends.
/// Dropping it ends the session, which stops the sandbox, and returns once the reader has sent
/// its last arrival, the end.
#[derive(Debug)]
pub struct Session {
    /// The session's number, from [`NEXT_SESSION`], which its ops and commands carry.
    id: u64,
    connection: UnixStream,
    // Shared with the reader thread, which routes the sandbox's frames by them, and removes a
    // route once its command has exited or nothing receives from it.
    routes: Arc<Mutex<Routes>>,
    // Shared with the reader thread, which may end the session. Set under the routes' lock,
    // which `end` explains.
    ended: Arc<OnceLock<String>>,
    /// The reader thread, `Some` until the drop joins it.
    reader: Option<JoinHandle<()>>,
}

/// Where frames from the sandbox go: the requests waiting for them and the commands running, by
/// stream, which streams have been opened, and whether the sandbox may open an op.
#[derive(Debug)]
struct Routes {
    open: BTreeMap<u32, Route>,
    /// The stream the next request opens: requests open streams in order from 1, so every
    /// stream from 1 below it has been opened.
    next_stream: u32,
    /// The op in flight, by number: opened, and not yet answered.
    op: Option<u32>,
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

/// A part of a tree being collected.
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

/// What the session's reader hands its owner ([`Listener::accept`]), in the order the sandbox
/// sent it, ending with the session's end, once.
#[derive(Debug)]
#[must_use = "an arrival may hold an op, and an op dropped unanswered wedges the sandbox's ops: \
              answer each with `Session::reply` or `Session::refuse`"]
pub enum Arrival {
    /// An op that arrived whole, for the owner to answer.
    Op(IncomingOp),
    /// An op whose payload was over the session's limit, for the owner to refuse.
    Refused(RefusedOp),
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
    /// The number of the session the op arrived on.
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

/// A command started in a sandbox, which its exit names ([`Arrival::Exited`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Process {
    /// The number of the session it runs on.
    session: u64,
    /// The stream it was started on, which its exit comes on.
    stream: u32,
}

impl Session {
    fn new(
        stream: UnixStream,
        limit: Option<OpPayloadLimit>,
        send: Box<dyn Fn(Arrival) + Send>,
    ) -> io::Result<Self> {
        let routes = Arc::new(Mutex::new(Routes {
            open: BTreeMap::new(),
            next_stream: 1,
            op: None,
        }));
        let ended = Arc::new(OnceLock::new());
        let incoming = stream
            .try_clone()
            .map_err(|error| context("the connection can't be cloned for its reader", &error))?;
        let (reader_routes, reader_ended) = (Arc::clone(&routes), Arc::clone(&ended));
        let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
        let assembler = Assembler {
            owner: Owner { session: id, send },
            limit,
            open: None,
        };
        // Reads the sandbox's frames while the owner writes requests and waits on their replies.
        let reader = thread::Builder::new()
            .name("session".to_owned())
            .spawn(move || route(&incoming, &reader_routes, &reader_ended, assembler))
            .map_err(|error| context("the session's reader thread didn't start", &error))?;
        Ok(Self {
            id,
            connection: stream,
            routes,
            ended,
            reader: Some(reader),
        })
    }

    /// Puts [`pumpkin_subdirs()`] under [`pumpkin_dir()`], which every sandbox needs before its
    /// first command, by `by` ([deadlines](crate#deadlines)).
    ///
    /// # Errors
    ///
    /// Fails if `sandboxd` can't put the directories, if `by` passes first, or if the session
    /// ends.
    pub fn set_up(&mut self, by: Deadline) -> Result<(), String> {
        let (root, subdirs) = (pumpkin_dir(), pumpkin_subdirs());
        let puts: Vec<Put<'_>> = subdirs.iter().map(Put::Dir).collect();
        self.put(&root, &puts, by).map_err(|error| {
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
    /// Each entry's parent is `dir` or comes earlier in `entries`, unless it exists already.
    ///
    /// # Errors
    ///
    /// Fails if `sandboxd` can't write them, if `by` passes first ([deadlines](crate#deadlines)),
    /// or if the session ends.
    pub fn put(&mut self, dir: &AbsolutePath, entries: &[Put<'_>], by: Deadline) -> io::Result<()> {
        let (stream, replies) = self.open(Route::Reply, by)?;
        let put = self
            .send_tree(stream, dir, entries, by)
            .and_then(|()| self.done(&replies, by));
        self.close(stream);
        put
    }

    /// Starts a command, whose exit arrives naming the [`Process`] this returns.
    ///
    /// # Errors
    ///
    /// Fails if the command doesn't start, if `by` passes first ([deadlines](crate#deadlines)),
    /// or if the session ends.
    pub fn start(&mut self, launch: Launch, by: Deadline) -> io::Result<Process> {
        let (stream, replies) = self.open(Route::Start, by)?;
        let started = self
            .send(&ToSandbox::Start { stream, launch }, &[], by)
            .and_then(|()| self.done(&replies, by));
        match started {
            // The reader removes the command's route at its exit, which outlives this call.
            Ok(()) => Ok(Process {
                session: self.id,
                stream,
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
    /// Fails if `sandboxd` can't kill them, if `by` passes first ([deadlines](crate#deadlines)),
    /// or if the session ends.
    pub fn freeze(&mut self, by: Deadline) -> io::Result<()> {
        self.request(|stream| ToSandbox::Freeze { stream }, by)
    }

    /// Copies the tree in the sandbox's `dir` into `into`, a new directory, within `limits`.
    /// After a failure, `into` holds whatever arrived before it.
    ///
    /// # Errors
    ///
    /// Fails if `into` exists, if `sandboxd` can't send the tree, if the tree passes a limit or
    /// can't be written, if `by` passes first ([deadlines](crate#deadlines)), or if the session
    /// ends, as a tree that breaks the protocol ends it.
    pub fn collect(
        &mut self,
        dir: &AbsolutePath,
        into: &Path,
        limits: Limits,
        by: Deadline,
    ) -> io::Result<()> {
        let (stream, parts) = self.open(Route::Tree, by)?;
        let created = fs::create_dir(into).map_err(|error| context(into.display(), &error));
        let collected = created.and_then(|()| {
            let tree = tree::Writer::new(into.to_owned(), limits);
            self.send(
                &ToSandbox::Collect {
                    stream,
                    dir: dir.clone(),
                },
                &[],
                by,
            )?;
            self.receive_tree(stream, &parts, tree, by)
        });
        self.close(stream);
        collected
    }

    /// Answers an op, which lets the sandbox send its next, as
    /// [the protocol](pumpkin_schema::sandbox) requires promptly. A reply too long for a frame
    /// reaches the caller as a refusal that says so.
    ///
    /// # Errors
    ///
    /// Fails if the reply is too long for a frame, if `by` passes first
    /// ([deadlines](crate#deadlines)), or if the session has ended.
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

    /// Answers an op whose payload was over the limit with its refusal, as [`Session::reply`]
    /// answers any other.
    ///
    /// # Errors
    ///
    /// Fails if `by` passes first ([deadlines](crate#deadlines)), or if the session has ended.
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
            #[expect(
                clippy::unreachable,
                reason = "the refusal's message is `RefusedOp::message`, which names only the \
                          payload's size, a `u64`, and the limit, so its frame's message is under \
                          200 bytes, far under `frame::MAX_MESSAGE_BYTES`, 64 KiB"
            )]
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                unreachable!("the refusal of op {number} is too long: {error}")
            }
            answered => answered,
        }
    }

    /// Takes op `number`, which arrived on session `session`, out of the flight, before its
    /// answer is written.
    fn clear_flight(&self, session: u64, number: u32) {
        #[expect(
            clippy::unreachable,
            reason = "the owner answers each op on the session that handed it out, as the \
                      contracts of `reply` and `refuse` require; a lifetime brand tying each op to \
                      its session would reshape all of `Session`'s `pub` API"
        )]
        if session != self.id {
            unreachable!("op {number} was answered on a session other than the one it arrived on")
        }
        // Before the answer, since the sandbox may open the next op as soon as it reads one,
        // which a later clearing would drop.
        lock(&self.routes).op = None;
    }

    /// Sends `reply` to op `number`, already out of the flight.
    fn answer(&mut self, number: u32, reply: OpReply, by: Deadline) -> io::Result<()> {
        self.can_send(by)?;
        let message = ToSandbox::OpReply { op: number, reply };
        match frame::write(&mut self.until(by), &message, &[]) {
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
        by: Deadline,
    ) -> io::Result<()> {
        self.send(
            &ToSandbox::Put {
                stream,
                dir: dir.clone(),
            },
            &[],
            by,
        )?;
        for entry in entries {
            match *entry {
                Put::Dir(path) => {
                    let entry = Entry::Dir { path: path.clone() };
                    self.send(&ToSandbox::Entry { stream, entry }, &[], by)?;
                }
                Put::File(path, bytes) => {
                    let size = bytes.len() as u64;
                    let entry = Entry::File {
                        path: path.clone(),
                        size,
                    };
                    self.send(&ToSandbox::Entry { stream, entry }, &[], by)?;
                    for chunk in bytes.chunks(frame::MAX_BODY_BYTES) {
                        self.send(&ToSandbox::Data { stream }, chunk, by)?;
                    }
                }
            }
        }
        self.send(&ToSandbox::End { stream }, &[], by)
    }

    /// Writes the tree whose parts arrive on `stream` with `tree`, until its end.
    fn receive_tree(
        &self,
        stream: u32,
        parts: &Receiver<Result<Part, String>>,
        mut tree: tree::Writer,
        by: Deadline,
    ) -> io::Result<()> {
        loop {
            match self.receive(parts, by)? {
                Ok(Part::Entry(entry)) => self.created(stream, tree.entry(&entry))?,
                Ok(Part::Data(bytes)) => self.written(stream, tree.data(&bytes))?,
                Ok(Part::End) => return self.written(stream, tree.finish()),
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
        end(&self.routes, &self.ended, &self.connection, reason);
        ended(&self.ended)
    }

    /// Sends the request `message` builds for a new stream, and waits for its reply.
    fn request(&mut self, message: impl FnOnce(u32) -> ToSandbox, by: Deadline) -> io::Result<()> {
        let (stream, replies) = self.open(Route::Reply, by)?;
        let done = self
            .send(&message(stream), &[], by)
            .and_then(|()| self.done(&replies, by));
        self.close(stream);
        done
    }

    /// Opens a stream for a request to send by `by`, whose replies `route` takes.
    fn open<R>(
        &mut self,
        route: fn(SyncSender<R>) -> Route,
        by: Deadline,
    ) -> io::Result<(u32, Receiver<R>)> {
        self.can_send(by)?;
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
            return Err(ended(&self.ended));
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
    fn send(&mut self, message: &ToSandbox, body: &[u8], by: Deadline) -> io::Result<()> {
        frame::write(&mut self.until(by), message, body).map_err(|error| self.write_failed(&error))
    }

    /// The connection, to write to until `by`.
    fn until(&self, by: Deadline) -> Until<'_> {
        Until {
            connection: &self.connection,
            by,
        }
    }

    /// Ends the session after a write that failed with `error`, which may leave part of a frame
    /// on the connection, and returns the request's error: the deadline's, if it has passed,
    /// or else the session's, whose reason is the failed write's unless it had ended already.
    fn write_failed(&self, error: &io::Error) -> io::Error {
        if matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ) {
            return self.timed_out(
                "the sandbox didn't take what was sent before the request's deadline: expected it \
                 to read while the deadline lasts",
            );
        }
        let reason = format!("writing to the sandbox failed: {error}");
        end(&self.routes, &self.ended, &self.connection, reason);
        ended(&self.ended)
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
                 while the deadline lasts",
            )),
            Err(RecvTimeoutError::Disconnected) => Err(ended(&self.ended)),
        }
    }

    /// Fails before a request is sent if the session has ended, for its reason, or else if `by`
    /// has passed, which ends the session though the sandbox has had no part in it.
    fn can_send(&self, by: Deadline) -> io::Result<()> {
        if self.ended.get().is_some() {
            return Err(ended(&self.ended));
        }
        if by.remaining().is_none() {
            return Err(self.timed_out(
                "the request's deadline passed before it was sent: expected time left to send it",
            ));
        }
        Ok(())
    }

    /// Ends the session for `reason`, as a request's deadline has passed, and returns the
    /// request's error.
    fn timed_out(&self, reason: &'static str) -> io::Error {
        end(
            &self.routes,
            &self.ended,
            &self.connection,
            reason.to_owned(),
        );
        io::Error::new(io::ErrorKind::TimedOut, reason)
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
        end(&self.routes, &self.ended, &self.connection, reason);
        // Nothing else holds the reader up: its sends to the owner never block; a tree's frames
        // wait only for a `collect`, which has returned, dropping their queue, before anything
        // can drop the session; and it takes the routes' lock only around steps that never
        // block, as this thread does, which holds it no longer.
        #[expect(
            clippy::unreachable,
            reason = "a panic on the reader thread aborts the process (`AbortOnUnwind` in `route`), \
                      so the thread never ends in one"
        )]
        if let Some(reader) = self.reader.take()
            && reader.join().is_err()
        {
            unreachable!("the session's reader thread panicked")
        }
    }
}

/// Writes to a connection until `by`, after which every write fails as timed out.
struct Until<'a> {
    connection: &'a UnixStream,
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

/// Reads frames from the sandbox until the session ends: each reply goes to the request
/// waiting on its stream, each op's frames to `ops`, and each command's exit, then the end, to
/// the owner.
fn route(
    connection: &UnixStream,
    routes: &Mutex<Routes>,
    ended: &OnceLock<String>,
    mut ops: Assembler,
) {
    // A panic here would skip `end`, leaving the session open with its owner waiting on it, so
    // it aborts the process instead, in every build, tests included.
    let unwinding = AbortOnUnwind;
    let mut reader = BufReader::new(connection);
    let reason = loop {
        let (message, body) = match frame::read::<FromSandbox>(&mut reader) {
            Ok(Some(frame)) => frame,
            Ok(None) => break "the sandbox ended the session".to_owned(),
            Err(error) => break format!("the session failed: {error}"),
        };
        let outcome = match message {
            FromSandbox::Op { op, call, size } => ops.open(routes, op, call, size),
            FromSandbox::OpData { op } => ops.data(op, &body),
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
    let reason = end(routes, ended, connection, reason).to_owned();
    ops.owner.arrived(Arrival::Ended { reason });
    mem::forget(unwinding);
}

/// The session's owner, as the reader thread reaches it.
struct Owner {
    /// The number of the session, which its ops and commands carry.
    session: u64,
    /// Sends an arrival to the owner, as [`Listener::accept`] was asked to.
    send: Box<dyn Fn(Arrival) + Send>,
}

impl Owner {
    fn arrived(&self, arrival: Arrival) {
        (self.send)(arrival);
    }
}

/// Aborts the process as it drops, which only a panic unwinding past it lets happen.
struct AbortOnUnwind;

impl Drop for AbortOnUnwind {
    fn drop(&mut self) {
        // Straight to the standard error, which `cargo test` doesn't capture, unlike the panic's
        // message from this thread.
        let _ = writeln!(
            io::stderr(),
            "the gateway's reader thread panicked, so the process aborts: under `cargo test`, \
             rerun with --nocapture to see the panic"
        );
        process::abort();
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

/// Assembles the op the sandbox sends, on the reader thread, and passes it on whole.
struct Assembler {
    /// The session's owner, which takes each op.
    owner: Owner,
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
        reason = "the size is at most the limit, which is at most `MAX_OP_PAYLOAD_BYTES`, 64 MiB \
                  (an `OpPayloadLimit` holds no more), which fits the usize of every Linux target, \
                  of 32 bits or more"
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
/// sandbox, records why in `ended`, and wakes every request waiting on it. Returns the reason
/// recorded: `reason`, unless the session had ended already.
fn end<'a>(
    routes: &Mutex<Routes>,
    ended: &'a OnceLock<String>,
    connection: &UnixStream,
    reason: String,
) -> &'a str {
    let mut routes = lock(routes);
    // Closed first, under the lock, so that no write reaches the sandbox once anything can
    // learn the session ended: an owner woken by the end may still answer an op. A connection
    // already closed is fine. The reason is set under the lock too, so that no route opens
    // after the routes are cleared. A reason already set is the session's.
    let _ = connection.shutdown(Shutdown::Both);
    let recorded = ended.get_or_init(|| reason);
    routes.open.clear();
    recorded
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    stdx::unpoisoned(mutex.lock())
}

/// The error of a request on a session that has ended, for the reason in `ended`.
fn ended(ended: &OnceLock<String>) -> io::Error {
    #[expect(
        clippy::unreachable,
        reason = "every way to learn that the session ended follows the reason's recording: `end` \
                  records it before it closes the routes, which close otherwise only after a frame \
                  that ends their request or once nothing receives from them; and the rest call \
                  `end` or check the reason first; sending the reason as each channel's last \
                  message would need room in queues that may be full"
    )]
    let Some(reason) = ended.get() else {
        unreachable!("the session ended without a reason")
    };
    io::Error::new(io::ErrorKind::ConnectionAborted, reason.clone())
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
