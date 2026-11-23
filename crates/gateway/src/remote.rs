//! A session driven from a thread other than its loop's: each call sends the loop a request,
//! which the loop's owner hands [`Sessions::serve`], and waits for its answer.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::time::Duration;

use schema::ir::AbsolutePath;
use schema::sandbox::op::OpReply;
use schema::sandbox::tree::Limits;
use schema::sandbox::{Entry, Launch, Root, ToSandbox};
use stdx::io::Io;

use crate::ask::{Asked, set_up_failed, set_up_puts};
use crate::{
    Answer, Answerer, IncomingModelRequest, IncomingOp, Owner, Process, Put, RefusedOp, SessionId,
    Sessions,
};

// How often a collect that its caller may stop looks whether it has: such a caller learns of its
// stop only from its own inbox, which the loop can't wake this wait for.
const LOOK: Duration = Duration::from_millis(10);

// How many entries a put takes from its caller at a time, as its session asks for them: each file
// among them holds a descriptor open until its bytes have gone, and puts that each took a whole
// tree's would run the process out of descriptors, whose soft limit is commonly 1024.
const PUT_BATCH: usize = 16;

/// What a [`Remote`] asks its session's loop, for the loop's owner to hand [`Sessions::serve`].
pub struct Request {
    session: SessionId,
    ask: Asked,
    answer: SyncSender<ToThread>,
    group: Option<u64>,
}

/// What the loop tells a thread that waits on it: the answer to its request, or that the put on
/// the stream takes its next entries.
pub(crate) enum ToThread {
    Answer(Answer),
    More(u32),
}

/// A session's handle on another thread than its loop's. Dropping it drops the session.
pub struct Remote {
    trees: Trees,
}

/// A handle that moves trees through a session from another thread, which doesn't own it.
pub struct Trees {
    session: SessionId,
    send: Arc<dyn Fn(Request) + Send + Sync>,
    answers: (SyncSender<ToThread>, Receiver<ToThread>),
    // What the loop's owner gives up this handle's collects by ([`Sessions::cancel`]), and what a
    // stop gives up.
    group: Option<u64>,
}

impl Clone for Trees {
    fn clone(&self) -> Self {
        Self {
            group: self.group,
            ..Self::new(self.session, Arc::clone(&self.send))
        }
    }
}

impl std::fmt::Debug for Trees {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Trees")
            .field("session", &self.session)
            .field("group", &self.group)
            .finish_non_exhaustive()
    }
}

impl Trees {
    pub(crate) fn of(session: SessionId, send: Arc<dyn Fn(Request) + Send + Sync>) -> Self {
        Self::new(session, send)
    }

    fn new(session: SessionId, send: Arc<dyn Fn(Request) + Send + Sync>) -> Self {
        Self {
            session,
            send,
            // Each call waits for its own answer, before which a put's session may ask it once for
            // more entries.
            answers: mpsc::sync_channel(2),
            group: None,
        }
    }

    /// This handle, whose collects the loop's owner gives up as it gives up `group`'s
    /// ([`Sessions::cancel`]), as a stop of a collect of the group's does on its session.
    #[must_use]
    pub fn in_group(mut self, group: u64) -> Self {
        self.group = Some(group);
        self
    }

    fn ask(&mut self, ask: Asked) -> Answer {
        self.send_ask(ask);
        self.answered(None)
    }

    // Panics, on the caller's thread, at an op or a command of another session's.
    fn send_ask(&self, ask: Asked) {
        ask.check_session(self.session);
        (self.send)(Request {
            session: self.session,
            ask,
            answer: self.answers.0.clone(),
            group: self.group,
        });
    }

    // Waits for what the loop tells this handle.
    fn told(&self) -> ToThread {
        match self.answers.1.recv() {
            Ok(told) => told,
            Err(_) => unreachable!(
                "a session's answers disconnected, though its handle keeps a sender of them"
            ),
        }
    }

    // Waits for the answer to the request sent; once `stopped` says so, gives the request up, and
    // takes the answer that gives it up.
    fn answered(&self, mut stopped: Option<&mut dyn FnMut() -> bool>) -> Answer {
        loop {
            let told = match &mut stopped {
                None => self.told(),
                Some(stop) => match self.answers.1.recv_timeout(LOOK) {
                    Ok(told) => told,
                    Err(RecvTimeoutError::Timeout) => {
                        if stop() {
                            stopped = None;
                            self.send_ask(Asked::Cancel(self.group));
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => unreachable!(
                        "a session's answers disconnected, though its handle keeps a sender of \
                         them"
                    ),
                },
            };
            match told {
                ToThread::Answer(answer) => return answer,
                ToThread::More(stream) => unreachable!(
                    "the put on stream {stream} asked for its next entries of a handle that waits \
                     on a request other than a put, though a handle waits on one at a time"
                ),
            }
        }
    }

    /// As [`Remote::put`], with `root` saying how `dir` is made. [`Root::New`] requires a new
    /// directory. It takes `entries` a few at a time, as it reaches them, and the tree ends where
    /// they end.
    pub fn put(
        &mut self,
        dir: &AbsolutePath,
        root: Root,
        entries: impl IntoIterator<Item = Put>,
        by: Duration,
    ) -> io::Result<()> {
        let mut entries = entries.into_iter();
        let (first, more) = batch(&mut entries);
        let more = more.then(|| self.answers.0.clone());
        let dir = dir.clone();
        self.send_ask(Asked::Put {
            dir,
            root,
            entries: first,
            more,
            by,
        });
        loop {
            match self.told() {
                ToThread::More(stream) => {
                    let (next, more) = batch(&mut entries);
                    self.send_ask(Asked::PutMore(stream, next, more));
                }
                ToThread::Answer(answer) => return done(answer),
            }
        }
    }

    /// Collects as [`Remote::collect`] does. Once `stopped` returns true, the collect fails with
    /// [`io::ErrorKind::Interrupted`], and the session stays open; `stopped` is asked every few
    /// milliseconds. So does every other collect on the session of a handle in this one's group
    /// ([`Trees::in_group`]), or in none if this one is in none.
    pub fn collect(
        &mut self,
        path: &AbsolutePath,
        into: &Path,
        limits: Limits,
        by: Duration,
        stopped: &mut dyn FnMut() -> bool,
    ) -> io::Result<Vec<Entry>> {
        let ask = Asked::Collect {
            path: path.clone(),
            into: into.to_owned(),
            limits,
            by,
        };
        self.send_ask(ask);
        collected(self.answered(Some(stopped)))
    }
}

// The next entries of a put to hand its session, and whether more may follow them.
fn batch(entries: &mut impl Iterator<Item = Put>) -> (Vec<Put>, bool) {
    let batch: Vec<Put> = entries.take(PUT_BATCH).collect();
    let more = batch.len() == PUT_BATCH;
    (batch, more)
}

fn done(answer: Answer) -> io::Result<()> {
    match answer {
        Answer::Done(done) => done,
        other => unreachable!("{other:?} answered a request that `done` answers"),
    }
}

fn collected(answer: Answer) -> io::Result<Vec<Entry>> {
    match answer {
        Answer::Collected(collected) => collected,
        other => unreachable!("{other:?} answered a collect"),
    }
}

impl Remote {
    pub(crate) fn new(session: SessionId, send: Arc<dyn Fn(Request) + Send + Sync>) -> Self {
        Self {
            trees: Trees::new(session, send),
        }
    }

    /// Why the session has ended, if it has.
    pub fn ended(&mut self) -> Option<String> {
        done(self.trees.ask(Asked::Ended))
            .err()
            .map(|error| error.to_string())
    }

    pub fn trees(&self) -> Trees {
        self.trees.clone()
    }

    /// Puts [`pumpkin_subdirs()`](schema::sandbox::pumpkin_subdirs) under
    /// [`pumpkin_dir()`](schema::sandbox::pumpkin_dir), with
    /// [`dir_mode()`](schema::sandbox::dir_mode), which every sandbox needs before its first
    /// command.
    pub fn set_up(&mut self, by: Duration) -> Result<(), String> {
        let (root, puts) = set_up_puts();
        self.put(&root, puts, by)
            .map_err(|error| set_up_failed(&root, &error))
    }

    /// Writes `entries` below `dir` in the sandbox, creating `dir` and its missing parents. Each
    /// entry's parent must be `dir` or come earlier in `entries`, unless it exists already. Returns
    /// at the first failure, and whatever was written before it stays.
    pub fn put(
        &mut self,
        dir: &AbsolutePath,
        entries: impl IntoIterator<Item = Put>,
        by: Duration,
    ) -> io::Result<()> {
        self.trees.put(dir, Root::Kept, entries, by)
    }

    /// Kills the sandbox's processes that [`ToSandbox::Freeze`] names, so that nothing collected
    /// afterwards changes, and returns once they are gone.
    pub fn freeze(&mut self, by: Duration) -> io::Result<()> {
        done(self.trees.ask(Asked::Freeze(by)))
    }

    /// Copies the tree at the sandbox's `path`, a directory's or a file's, into `into`, a new
    /// directory, within `limits`. Fails if `into` exists or the tree passes a limit; after any
    /// failure, `into` holds whatever arrived before it.
    pub fn collect(
        &mut self,
        path: &AbsolutePath,
        into: &Path,
        limits: Limits,
        by: Duration,
    ) -> io::Result<()> {
        let ask = Asked::Collect {
            path: path.clone(),
            into: into.to_owned(),
            limits,
            by,
        };
        collected(self.trees.ask(ask)).map(drop)
    }

    /// Starts a command. Its [`crate::Arrival::Exited`] names the returned process.
    pub fn start(&mut self, launch: Launch, by: Duration) -> io::Result<Process> {
        match self.trees.ask(Asked::Start(launch, by)) {
            Answer::Started(started) => started,
            other => unreachable!("{other:?} answered a start"),
        }
    }

    /// Kills `process`'s group ([`ToSandbox::Kill`]), whose exit then arrives as any other's.
    ///
    /// # Panics
    ///
    /// Panics if `process` was started on another session.
    pub fn kill(&mut self, process: Process, by: Duration) -> io::Result<()> {
        done(self.trees.ask(Asked::Kill(process, by)))
    }

    /// Answers an op, promptly: the sandbox opens no other until it is. A `reply` too long for a
    /// frame refuses the op instead, and fails.
    ///
    /// # Panics
    ///
    /// Panics if `op` arrived on another session.
    pub fn reply(&mut self, op: IncomingOp, reply: OpReply, by: Duration) -> io::Result<()> {
        done(self.trees.ask(Asked::Reply(op, reply, by)))
    }

    /// Answers an op whose payload was over the limit with its refusal.
    ///
    /// # Panics
    ///
    /// Panics if `refused` arrived on another session.
    pub fn refuse(&mut self, refused: RefusedOp, by: Duration) -> io::Result<()> {
        done(self.trees.ask(Asked::Refuse(refused, by)))
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.trees.send_ask(Asked::Drop);
    }
}

/// Where a model request's response goes, from whatever thread answers it.
#[derive(Debug)]
pub(crate) struct Responder {
    trees: Trees,
    request: u32,
}

impl Responder {
    pub(crate) fn new(
        session: SessionId,
        request: u32,
        send: Arc<dyn Fn(Request) + Send + Sync>,
    ) -> Self {
        Self {
            trees: Trees::new(session, send),
            request,
        }
    }

    fn send(&mut self, message: ToSandbox, body: Vec<u8>, by: Duration) -> io::Result<()> {
        done(self.trees.ask(Asked::Respond(message, body, by)))
    }
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
    by: Duration,
    finished: bool,
}

impl IncomingModelRequest {
    /// Starts the response with `status` and `headers`, and returns it, to stream its body by
    /// `by`, a time since the sessions' origin. Fails if the head is too long for a frame, which
    /// cuts the request short without a response.
    pub fn respond(
        self,
        status: u16,
        headers: Vec<(String, String)>,
        by: Duration,
    ) -> io::Result<ModelResponse> {
        let Self {
            request: _,
            responder,
        } = self;
        let request = responder.request;
        let mut response = ModelResponse {
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
        response.responder.send(head, Vec::new(), by)?;
        Ok(response)
    }
}

impl ModelResponse {
    pub fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        let request = self.responder.request;
        for chunk in bytes.chunks(schema::frame::MAX_BODY_BYTES) {
            let data = ToSandbox::ModelData { request };
            self.responder.send(data, chunk.to_vec(), self.by)?;
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
        let end = ToSandbox::ModelEnd { request, cut };
        self.responder.send(end, Vec::new(), self.by)
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

impl Sessions {
    /// Serves `request`, which a [`Remote`] on another thread sent this loop.
    pub fn serve<C: Owner>(owner: &mut C, io: &mut Io<C>, request: Request) {
        let Request {
            session,
            ask,
            answer,
            group,
        } = request;
        Self::dispatch_to(owner, io, session, ask, Answerer::Thread(answer, group));
        Self::hand_over(owner, io);
    }
}
