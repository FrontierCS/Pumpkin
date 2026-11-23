//! What a session's owner asks of the sandbox, each request on a stream of its own, and the
//! frames that answer the sandbox's ops and model requests.

use std::io;
use std::path::PathBuf;
use std::sync::mpsc::SyncSender;
use std::time::Duration;

use schema::sandbox::op::OpReply;
use schema::sandbox::tree::{self, Limits};
use schema::sandbox::{Launch, ToSandbox};
use stdx::io::Io;

use super::replies::Route;
use super::{Session, Then};
use crate::remote::ToThread;
use crate::trees::PutState;
use crate::{Answer, Answerer, By, Due, IncomingOp, Owner, Process, Put, RefusedOp, Told, frames};

impl Session {
    /// Gives up each collect that a handle in `group` waits for, which fails as interrupted, and
    /// leaves the session open; none is the group of every handle outside one.
    pub(crate) fn cancel_group(&mut self, group: Option<u64>, told: &mut Told<'_>) {
        let streams: Vec<u32> = (self.routes.iter())
            .filter_map(|(stream, route)| match route {
                Route::Tree {
                    answer: Answerer::Thread(_, of),
                    ..
                } if *of == group => Some(*stream),
                _ => None,
            })
            .collect();
        for stream in streams {
            if let Some(Route::Tree { answer, .. }) = self.routes.remove(&stream) {
                let cut = "stopped before the tree was whole";
                let error = io::Error::new(io::ErrorKind::Interrupted, cut);
                told.answer(answer, Answer::Collected(Err(error)));
            }
        }
    }

    fn check_open(&self) -> io::Result<()> {
        if self.ended.is_some() {
            return Err(self.ended_error());
        }
        Ok(())
    }

    // Opens the next stream for a request, whose message `message` makes for it and which follows
    // with what `then` makes for it, by `by`: the stream, or why the request failed.
    fn open_request<C: Owner>(
        &mut self,
        io: &mut Io<C>,
        message: impl FnOnce(u32) -> ToSandbox,
        (by, then): (Duration, fn(u32) -> Then),
        told: &mut Told<'_>,
    ) -> io::Result<u32> {
        let stream = self.next_stream;
        self.next_stream = stream.wrapping_add(1);
        let queued = self.queue(
            io,
            (&message(stream), &[]),
            By::request(by),
            then(stream),
            told,
        );
        queued.map(|()| stream).map_err(|(error, _)| error)
    }

    /// Sends a request that `done` or `failed` answers, made by `message` for its stream.
    pub(crate) fn request<C: Owner>(
        &mut self,
        io: &mut Io<C>,
        message: impl FnOnce(u32) -> ToSandbox,
        (answer, by): (Answerer, Duration),
        told: &mut Told<'_>,
    ) {
        match self.open_request(io, message, (by, |_| Then::Nothing), told) {
            Ok(stream) => {
                self.routes.insert(stream, Route::Reply { answer, by });
            }
            Err(error) => told.answer(answer, Answer::Done(Err(error))),
        }
    }

    pub(crate) fn start<C: Owner>(
        &mut self,
        io: &mut Io<C>,
        launch: Launch,
        (answer, by): (Answerer, Duration),
        told: &mut Told<'_>,
    ) -> Option<Process> {
        let message = |stream| ToSandbox::Start { stream, launch };
        match self.open_request(io, message, (by, |_| Then::Nothing), told) {
            Ok(stream) => {
                let process = Process {
                    session: self.id,
                    stream,
                };
                let route = Route::Start {
                    answer,
                    by,
                    process,
                };
                self.routes.insert(stream, route);
                Some(process)
            }
            Err(error) => {
                told.answer(answer, Answer::Started(Err(error)));
                None
            }
        }
    }

    pub(crate) fn kill<C: Owner>(
        &mut self,
        io: &mut Io<C>,
        process: Process,
        (answer, by): (Answerer, Duration),
        told: &mut Told<'_>,
    ) {
        let kill = |request| ToSandbox::Kill {
            stream: request,
            command: process.stream,
        };
        self.request(io, kill, (answer, by), told);
    }

    pub(crate) fn collect<C: Owner>(
        &mut self,
        io: &mut Io<C>,
        (path, into, limits): (schema::ir::AbsolutePath, PathBuf, Limits),
        (answer, by): (Answerer, Duration),
        told: &mut Told<'_>,
    ) {
        // Before its directory is made, which a session that has ended makes not.
        if let Err(error) = self.check_open() {
            return told.answer(answer, Answer::Collected(Err(error)));
        }
        if let Err(error) = std::fs::create_dir(&into) {
            let error = crate::context(into.display(), &error);
            return told.answer(answer, Answer::Collected(Err(error)));
        }
        let message = |stream| ToSandbox::Collect { stream, path };
        match self.open_request(io, message, (by, |_| Then::Nothing), told) {
            Ok(stream) => {
                let tree = tree::Writer::new(into, limits, tree::Modes::Umasked);
                let route = Route::Tree {
                    answer,
                    by,
                    tree,
                    entries: Vec::new(),
                };
                self.routes.insert(stream, route);
            }
            Err(error) => told.answer(answer, Answer::Collected(Err(error))),
        }
    }

    /// Puts `entries` below `dir`, made as `root` says. Where `more` is given, the put asks there
    /// for its next entries once it has sent these, and takes them by [`Session::put_more`].
    pub(crate) fn put<C: Owner>(
        &mut self,
        io: &mut Io<C>,
        (dir, root, entries, more): (
            schema::ir::AbsolutePath,
            schema::sandbox::Root,
            Vec<Put>,
            Option<SyncSender<ToThread>>,
        ),
        (answer, by): (Answerer, Duration),
        told: &mut Told<'_>,
    ) {
        let message = |stream| ToSandbox::Put { stream, dir, root };
        match self.open_request(io, message, (by, Then::PutFrame), told) {
            Ok(stream) => {
                let put = PutState::new(entries, more);
                self.routes.insert(stream, Route::Put { answer, by, put });
            }
            Err(error) => told.answer(answer, Answer::Done(Err(error))),
        }
    }

    /// Hands the put on `stream` the next entries that it asked for, after which more follow if
    /// `more`. A put already answered drops them.
    pub(crate) fn put_more<C: Owner>(
        &mut self,
        io: &mut Io<C>,
        stream: u32,
        (entries, more): (Vec<Put>, bool),
        told: &mut Told<'_>,
    ) {
        if let Some(Route::Put { put, .. }) = self.routes.get_mut(&stream) {
            put.hand(entries, more);
            self.pump_put(io, stream, told);
        }
    }

    // Takes the send of one of the put's frames, and queues its next ones.
    pub(super) fn put_sent<C: Owner>(&mut self, io: &mut Io<C>, stream: u32, told: &mut Told<'_>) {
        // A put answered already sends nothing more.
        if let Some(Route::Put { put, .. }) = self.routes.get_mut(&stream) {
            put.sent();
            self.pump_put(io, stream, told);
        }
    }

    // Queues the put's next frames, as many as it holds queued at most.
    fn pump_put<C: Owner>(&mut self, io: &mut Io<C>, stream: u32, told: &mut Told<'_>) {
        while self.ended.is_none() {
            let Some(Route::Put { put, by, .. }) = self.routes.get_mut(&stream) else {
                return;
            };
            let by = By::request(*by);
            let Some(frame) = put.next(stream) else {
                return;
            };
            let then = Then::PutFrame(stream);
            let (message, body) = frame;
            match self.queue(io, (&message, &body), by, then, told) {
                Ok(()) => {}
                Err((error, _)) if error.kind() == io::ErrorKind::InvalidInput => {
                    unreachable!("a put's frame is too long: {error}")
                }
                // The session has ended, which answered the put.
                Err(_) => return,
            }
        }
    }

    pub(crate) fn reply<C: Owner>(
        &mut self,
        io: &mut Io<C>,
        op: IncomingOp,
        reply: OpReply,
        (answer, by): (Answerer, Duration),
        told: &mut Told<'_>,
    ) {
        let IncomingOp {
            session: _,
            number,
            op,
        } = op;
        // Its payload goes first, so that the next op's buffer is never held beside it while the
        // write waits.
        drop(op);
        self.flight.op = None;
        let message = ToSandbox::OpReply { op: number, reply };
        let (message, then) = match frames::bytes(&message, &[]) {
            Ok(_) => (message, Then::Answer(answer)),
            // A reply too long for a frame refuses the op instead, and fails.
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                let message = format!("the reply was too long to send: {error}");
                let refusal = ToSandbox::OpReply {
                    op: number,
                    reply: OpReply::refused(message),
                };
                (refusal, Then::Fails(answer, error))
            }
            Err(error) => return told.answer(answer, Answer::Done(Err(error))),
        };
        if let Err((error, then)) = self.queue(io, (&message, &[]), By::request(by), then, told) {
            self.finished(io, then, Err(error), told);
        }
    }

    pub(crate) fn refuse<C: Owner>(
        &mut self,
        io: &mut Io<C>,
        refused: &RefusedOp,
        (answer, by): (Answerer, Duration),
        told: &mut Told<'_>,
    ) {
        self.flight.op = None;
        let refusal = ToSandbox::OpReply {
            op: refused.number,
            reply: OpReply::refused(refused.message()),
        };
        let then = Then::Answer(answer);
        if let Err((error, then)) = self.queue(io, (&refusal, &[]), By::request(by), then, told) {
            self.finished(io, then, Err(error), told);
        }
    }

    /// Queues a frame of a model response, answered once it is sent.
    pub(crate) fn respond<C: Owner>(
        &mut self,
        io: &mut Io<C>,
        (message, body): (ToSandbox, Vec<u8>),
        (answer, by): (Answerer, Duration),
        told: &mut Told<'_>,
    ) {
        if let ToSandbox::ModelEnd { request, .. } = &message {
            // Out of flight before the end is sent: the sandbox may reuse the number once it reads
            // the end.
            self.flight.models.remove(request);
        }
        let by = By {
            at: by,
            due: Due::Response,
        };
        let then = Then::Answer(answer);
        if let Err((error, then)) = self.queue(io, (&message, &body), by, then, told) {
            self.finished(io, then, Err(error), told);
        }
    }
}
