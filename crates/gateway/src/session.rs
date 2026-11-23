//! One session's state on its loop: what it receives and reads into frames, what it sends, and
//! where each frame from the sandbox goes.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::time::Duration;

use schema::ir::GpuModel;
use schema::sandbox::{FromSandbox, OpPayloadLimit, ToSandbox};
use stdx::io::{self as sio, Io};

use crate::{
    Answer, Answerer, Arrival, By, Flight, ModelRequest, Owner, SANDBOX_ENDED, SessionId, Sink,
    SocketMeeting, Told, frames, remote,
};

mod arrivals;
mod hello;
mod replies;
mod requests;

use arrivals::Assembly;
use replies::{Reply, Route};

// How many bytes a receive takes at most: a quarter of a frame's largest body, so that a body
// takes few receives, and a session holds little beside the frame it reads.
const RECEIVE_BYTES: usize = 256 * 1024;

#[expect(
    clippy::struct_excessive_bools,
    reason = "each is a fact of its own: whether it takes model requests, whether its owner \
              dropped it, and whether a receive, a send or an accept is in flight"
)]
pub(crate) struct Session {
    pub(crate) id: SessionId,
    origin: Duration,
    stream: Option<sio::Stream>,
    // The TLS stream whose relay carries the connection, kept as long as the stream, which says why
    // TLS ended.
    pub(crate) kept: Option<tls::Stream>,
    pub(crate) phase: Phase,
    gpus: Option<Vec<GpuModel>>,
    pub(crate) sink: Sink,
    limit: Option<OpPayloadLimit>,
    takes_models: bool,
    // Whether a receive, a send or an accept is in flight.
    receiving: bool,
    sending: bool,
    accepting: bool,
    // The buffer that the next receive takes, which each receive's callback hands back.
    receive_buffer: Vec<u8>,
    inbound: frames::Inbound,
    outbound: VecDeque<Out>,
    routes: BTreeMap<u32, Route>,
    // Requests take streams in order from 1, so every stream from 1 below it was a request's.
    next_stream: u32,
    flight: Flight,
    assembly: Option<(u32, Assembly)>,
    ended: Option<String>,
    // The time by which the requests whose deadline passed as the session ended were due.
    late: Option<Duration>,
    dropped: bool,
    pub(crate) remote: std::sync::Arc<dyn Fn(remote::Request) + Send + Sync>,
}

pub(crate) enum Phase {
    /// Waiting for the sandbox to connect to `listener`, then for its hello.
    Accepting {
        listener: Option<SocketMeeting>,
        by: Duration,
        answer: Option<Answerer>,
    },
    /// The hello is answered once the answer is sent.
    Welcoming {
        by: Duration,
        answer: Option<Answerer>,
    },
    Open,
}

// A frame to send, from the first of its bytes still to go, by when, and what follows once it is
// sent.
struct Out {
    bytes: Vec<u8>,
    from: usize,
    by: By,
    started: bool,
    then: Then,
}

enum Then {
    Nothing,
    Answer(Answerer),
    // Answers with the error once the frame is sent, or with the send's own failure.
    Fails(Answerer, io::Error),
    Welcomed,
    Refused(io::Error),
    PutFrame(u32),
}

pub(crate) struct Opening {
    pub(crate) limit: Option<OpPayloadLimit>,
    pub(crate) takes_models: bool,
    pub(crate) sink: Sink,
}

impl Session {
    pub(crate) fn new(
        id: SessionId,
        origin: Duration,
        phase: Phase,
        opening: Opening,
        remote: std::sync::Arc<dyn Fn(remote::Request) + Send + Sync>,
    ) -> Self {
        let Opening {
            limit,
            takes_models,
            sink,
        } = opening;
        Self {
            id,
            origin,
            stream: None,
            kept: None,
            // A session that accepts its sandbox opens with the accept in flight.
            accepting: matches!(phase, Phase::Accepting { .. }),
            phase,
            gpus: None,
            sink,
            limit,
            takes_models,
            receiving: false,
            sending: false,
            receive_buffer: Vec::new(),
            inbound: frames::Inbound::default(),
            outbound: VecDeque::new(),
            routes: BTreeMap::new(),
            next_stream: 1,
            flight: Flight::default(),
            assembly: None,
            ended: None,
            late: None,
            dropped: false,
            remote,
        }
    }

    pub(crate) fn ended(&self) -> Option<&str> {
        self.ended.as_deref()
    }

    pub(crate) fn gpus(&self) -> Option<&[GpuModel]> {
        self.gpus.as_deref()
    }

    pub(crate) fn now<C: Owner>(&self, io: &Io<C>) -> Duration {
        io.now().saturating_sub(self.origin)
    }

    /// Whether its owner has dropped it and nothing is in flight on it.
    pub(crate) fn done(&self) -> bool {
        self.dropped && !self.receiving && !self.sending && !self.accepting
    }

    pub(crate) fn close<C: Owner>(mut self, io: &mut Io<C>) {
        if let Some(stream) = self.stream.take() {
            io.close(stream);
        }
        self.close_listener(io);
    }

    /// The earliest deadline it waits on, as a time since the sessions' origin.
    pub(crate) fn due(&self) -> Option<Duration> {
        if self.ended.is_some() {
            return None;
        }
        let phase = match &self.phase {
            Phase::Accepting { by, .. } | Phase::Welcoming { by, .. } => Some(*by),
            Phase::Open => None,
        };
        let routes = self.routes.values().filter_map(Route::by);
        let out = self.outbound.front().map(|out| out.by.at);
        phase.into_iter().chain(routes).chain(out).min()
    }

    pub(crate) fn stream(&mut self, stream: sio::Stream) {
        self.stream = Some(stream);
    }

    pub(crate) fn set_gpus(&mut self, gpus: Option<Vec<GpuModel>>) {
        self.gpus = gpus;
    }

    // Starts the reads of what the sandbox sends.
    pub(crate) fn receive<C: Owner>(&mut self, io: &mut Io<C>) {
        if self.receiving || self.ended.is_some() {
            return;
        }
        let Some(stream) = &self.stream else {
            return;
        };
        self.receiving = true;
        let mut buffer = std::mem::take(&mut self.receive_buffer);
        if buffer.is_empty() {
            buffer = vec![0; RECEIVE_BYTES];
        }
        io.recv(stream, buffer, self.id.token(), on_received::<C>);
    }

    /// Ends the session, for `reason` unless it ended already: shuts its connection down, answers
    /// each request waiting with the reason, and tells the sink its end.
    pub(crate) fn end<C: Owner>(&mut self, io: &mut Io<C>, reason: String, told: &mut Told<'_>) {
        self.end_by(io, (reason, None), told);
    }

    // Ends the session for `reason`, as `end` does; a request whose deadline is `late` or earlier
    // fails as timed out, the rest as aborted.
    fn end_by<C: Owner>(
        &mut self,
        io: &mut Io<C>,
        (reason, late): (String, Option<Duration>),
        told: &mut Told<'_>,
    ) {
        if self.ended.is_some() {
            return;
        }
        if let Some(stream) = &self.stream {
            io.shutdown(stream);
        }
        let aborted = || io::Error::new(io::ErrorKind::ConnectionAborted, reason.clone());
        match std::mem::replace(&mut self.phase, Phase::Open) {
            Phase::Accepting {
                answer, listener, ..
            } => {
                if let Some(answer) = answer {
                    told.answer(answer, Answer::Accepted(Err(aborted())));
                }
                self.phase = Phase::Accepting {
                    listener,
                    by: Duration::ZERO,
                    answer: None,
                };
            }
            Phase::Welcoming { answer, .. } => {
                if let Some(answer) = answer {
                    told.answer(answer, Answer::Accepted(Err(aborted())));
                }
            }
            Phase::Open => {}
        }
        self.ended = Some(reason.clone());
        self.late = late;
        let failed = |by: Duration| {
            let kind = match late {
                Some(late) if by <= late => io::ErrorKind::TimedOut,
                _ => io::ErrorKind::ConnectionAborted,
            };
            io::Error::new(kind, reason.clone())
        };
        for (_, route) in std::mem::take(&mut self.routes) {
            match route {
                Route::Reply { answer, by } | Route::Put { answer, by, .. } => {
                    told.answer(answer, Answer::Done(Err(failed(by))));
                }
                Route::Start { answer, by, .. } => {
                    told.answer(answer, Answer::Started(Err(failed(by))));
                }
                Route::Tree { answer, by, .. } => {
                    told.answer(answer, Answer::Collected(Err(failed(by))));
                }
                Route::Running => {}
            }
        }
        // The frame being sent finishes or fails as its send completes; the rest never go.
        let keep = usize::from(self.sending);
        for out in self.outbound.drain(keep..) {
            if let Then::Answer(answer) | Then::Fails(answer, _) = out.then {
                told.answer(answer, Answer::Done(Err(failed(out.by.at))));
            }
        }
        told.arrived(&self.sink, Arrival::Ended { reason });
        self.close_when_idle(io);
    }

    fn ended_error(&self) -> io::Error {
        let Some(reason) = &self.ended else {
            unreachable!("a session's end was asked for before it ended")
        };
        io::Error::new(io::ErrorKind::ConnectionAborted, reason.clone())
    }

    pub(crate) fn drop_by_owner<C: Owner>(&mut self, io: &mut Io<C>, told: &mut Told<'_>) {
        self.dropped = true;
        self.end(io, "the owner dropped the session".to_owned(), told);
        // An accept in flight ends as its listener closes.
        self.close_listener(io);
    }

    pub(crate) fn close_listener<C: Owner>(&mut self, io: &mut Io<C>) {
        if let Phase::Accepting { listener, .. } = &mut self.phase
            && let Some(meeting) = listener.take()
        {
            meeting.withdraw(io);
        }
    }

    // Closes the stream once nothing is in flight on it, after the session has ended.
    fn close_when_idle<C: Owner>(&mut self, io: &mut Io<C>) {
        if self.ended.is_some()
            && !self.receiving
            && !self.sending
            && let Some(stream) = self.stream.take()
        {
            io.close(stream);
        }
    }

    /// Ends the session once a deadline that it waits on has passed by `now`.
    pub(crate) fn tick<C: Owner>(&mut self, io: &mut Io<C>, now: Duration, told: &mut Told<'_>) {
        if self.ended.is_some() {
            return;
        }
        let late = |by: Duration| now >= by;
        match &self.phase {
            Phase::Accepting { by, listener, .. } if late(*by) => {
                let reason = match listener {
                    Some(meeting) => format!(
                        "no sandbox connected to {} by the deadline: expected its sandbox to \
                         connect while the deadline lasts",
                        meeting.path.display()
                    ),
                    None => "the sandbox connected but sent no hello by the deadline: expected \
                             its sandboxd to send one as it connects, as a sandboxd built from \
                             the same Pumpkin does"
                        .to_owned(),
                };
                self.close_listener(io);
                return self.fail_accept(io, io::Error::new(io::ErrorKind::TimedOut, reason), told);
            }
            Phase::Welcoming { by, .. } if late(*by) => {
                let reason = "the deadline passed before the sandbox's hello was answered: \
                              expected time left to answer it";
                let error = io::Error::new(io::ErrorKind::TimedOut, reason);
                return self.fail_accept(io, error, told);
            }
            Phase::Accepting { .. } | Phase::Welcoming { .. } | Phase::Open => {}
        }
        // A frame that the sandbox doesn't take is why its request goes unanswered.
        if let Some(out) = self.outbound.front()
            && late(out.by.at)
        {
            let reason = if out.started {
                format!(
                    "the sandbox didn't take what was sent before the {}'s deadline: expected it \
                     to read while the deadline lasts",
                    out.by.due.noun()
                )
            } else {
                format!(
                    "the {}'s deadline passed before it was sent: expected time left to send it",
                    out.by.due.noun()
                )
            };
            return self.end_by(io, (reason, Some(now)), told);
        }
        if self.routes.values().filter_map(Route::by).any(late) {
            let reason = "the sandbox didn't answer before the request's deadline: expected its \
                          answer while the deadline lasts";
            self.end_by(io, (reason.to_owned(), Some(now)), told);
        }
    }

    /// Queues a frame to send by `by`, which follows with `then`. Fails, sending nothing and
    /// giving `then` back, if the session has ended or `by` has passed, which ends it; a frame too
    /// long for the protocol fails with `InvalidInput` and leaves the session open.
    fn queue<C: Owner>(
        &mut self,
        io: &mut Io<C>,
        (message, body): (&ToSandbox, &[u8]),
        by: By,
        then: Then,
        told: &mut Told<'_>,
    ) -> Result<(), (io::Error, Then)> {
        if self.ended.is_some() {
            return Err((self.ended_error(), then));
        }
        if self.now(io) >= by.at {
            let reason = format!(
                "the {}'s deadline passed before it was sent: expected time left to send it",
                by.due.noun()
            );
            self.end(io, reason.clone(), told);
            return Err((io::Error::new(io::ErrorKind::TimedOut, reason), then));
        }
        match frames::bytes(message, body) {
            Ok(bytes) => {
                self.push(io, bytes, by, then);
                Ok(())
            }
            Err(error) => Err((error, then)),
        }
    }

    fn push<C: Owner>(&mut self, io: &mut Io<C>, bytes: Vec<u8>, by: By, then: Then) {
        self.outbound.push_back(Out {
            bytes,
            from: 0,
            by,
            started: false,
            then,
        });
        self.send_next(io);
    }

    fn send_next<C: Owner>(&mut self, io: &mut Io<C>) {
        if self.sending || self.ended.is_some() {
            return;
        }
        let (Some(stream), Some(out)) = (&self.stream, self.outbound.front_mut()) else {
            return;
        };
        let bytes = std::mem::take(&mut out.bytes);
        out.started = true;
        self.sending = true;
        io.send(stream, (bytes, out.from), self.id.token(), on_sent::<C>);
    }

    fn sent<C: Owner>(
        &mut self,
        io: &mut Io<C>,
        buffer: Vec<u8>,
        result: io::Result<usize>,
        told: &mut Told<'_>,
    ) {
        self.sending = false;
        if self.ended.is_some() {
            if let Some(out) = self.outbound.pop_front() {
                // The frame whose deadline passed as it was sent ends as timed out.
                let mut error = self.ended_error();
                if self.late.is_some_and(|late| out.by.at <= late) {
                    error = io::Error::new(io::ErrorKind::TimedOut, error.to_string());
                }
                self.finished(io, out.then, Err(error), told);
            }
            return self.close_when_idle(io);
        }
        let sent = match result {
            Ok(sent) => sent,
            Err(error) => {
                let reason = if closed_by_sandbox(&error) {
                    SANDBOX_ENDED.to_owned()
                } else {
                    format!("writing to the sandbox failed: {error}")
                };
                if let Some(out) = self.outbound.pop_front() {
                    self.outbound.push_front(Out {
                        bytes: Vec::new(),
                        ..out
                    });
                }
                return self.end(io, reason, told);
            }
        };
        let Some(out) = self.outbound.front_mut() else {
            unreachable!("a send completed with nothing to send")
        };
        out.from += sent;
        if out.from < buffer.len() {
            out.bytes = buffer;
            return self.send_next(io);
        }
        let Some(out) = self.outbound.pop_front() else {
            unreachable!("the frame just sent is first to send")
        };
        self.finished(io, out.then, Ok(()), told);
        self.send_next(io);
    }

    // What follows a frame's send.
    fn finished<C: Owner>(
        &mut self,
        io: &mut Io<C>,
        then: Then,
        result: io::Result<()>,
        told: &mut Told<'_>,
    ) {
        match then {
            Then::Nothing => {}
            Then::Answer(answer) => told.answer(answer, Answer::Done(result)),
            Then::Fails(answer, error) => told.answer(answer, Answer::Done(result.and(Err(error)))),
            Then::Welcomed => {
                let Phase::Welcoming { answer, .. } =
                    std::mem::replace(&mut self.phase, Phase::Open)
                else {
                    return;
                };
                match result {
                    Ok(()) => {
                        if let Some(answer) = answer {
                            told.answer(answer, Answer::Accepted(Ok(self.id)));
                        }
                        self.read_frames(io, told);
                    }
                    Err(error) => {
                        self.phase = Phase::Welcoming {
                            by: Duration::ZERO,
                            answer,
                        };
                        let error = crate::context(
                            "the answer to the sandbox's hello can't be sent",
                            &error,
                        );
                        self.fail_accept(io, error, told);
                    }
                }
            }
            Then::Refused(error) => self.fail_accept(io, error, told),
            Then::PutFrame(stream) => self.put_sent(io, stream, told),
        }
    }

    pub(crate) fn received<C: Owner>(
        &mut self,
        io: &mut Io<C>,
        buffer: Vec<u8>,
        result: io::Result<usize>,
        told: &mut Told<'_>,
    ) {
        self.receiving = false;
        if self.ended.is_some() {
            return self.close_when_idle(io);
        }
        match result {
            Ok(0) => self.stream_ended(io, told),
            Ok(read) => {
                self.inbound.extend(&buffer[..read]);
                self.receive_buffer = buffer;
                match &self.phase {
                    Phase::Accepting { .. } => self.read_hello(io, told),
                    Phase::Open => self.read_frames(io, told),
                    Phase::Welcoming { .. } => {}
                }
                self.receive(io);
            }
            Err(error) if closed_by_sandbox(&error) => self.stream_ended(io, told),
            Err(error) => {
                let reason = format!("the session failed: {error}");
                match &self.phase {
                    Phase::Open => self.end(io, reason, told),
                    Phase::Accepting { .. } | Phase::Welcoming { .. } => {
                        let error = crate::context("the sandbox's hello can't be read", &error);
                        self.fail_accept(io, error, told);
                    }
                }
            }
        }
    }

    fn stream_ended<C: Owner>(&mut self, io: &mut Io<C>, told: &mut Told<'_>) {
        match &self.phase {
            Phase::Open => {
                let cut = frames::cut_short(self.inbound.rest(), |rest| {
                    schema::frame::read::<FromSandbox>(rest)
                });
                // At the end of a relayed stream, TLS says why the connection ended, unless the
                // peer closed it cleanly.
                let tls = self.kept.as_ref().and_then(tls::Stream::ended);
                let reason = match (cut, tls) {
                    (_, Some(error)) if !closed_by_sandbox(&error) => {
                        format!("the session failed: {error}")
                    }
                    (Some(error), _) => format!("the session failed: {error}"),
                    (None, _) => SANDBOX_ENDED.to_owned(),
                };
                self.end(io, reason, told);
            }
            Phase::Accepting { .. } | Phase::Welcoming { .. } => {
                let message = if self.inbound.rest().is_empty() {
                    "the sandbox closed the connection before its hello: expected it to open its \
                     session with one"
                } else {
                    "the sandbox closed the connection inside its first frame: expected a whole \
                     hello"
                };
                let error = io::Error::new(io::ErrorKind::UnexpectedEof, message);
                self.fail_accept(io, error, told);
            }
        }
    }

    fn read_frames<C: Owner>(&mut self, io: &mut Io<C>, told: &mut Told<'_>) {
        while self.ended.is_none() {
            let (message, body) = match self.inbound.take_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => return,
                Err(error) => return self.end(io, format!("the session failed: {error}"), told),
            };
            if let Err(reason) = self.frame(io, message, body, told) {
                return self.end(io, reason, told);
            }
        }
    }

    fn frame<C: Owner>(
        &mut self,
        io: &mut Io<C>,
        message: FromSandbox,
        body: Vec<u8>,
        told: &mut Told<'_>,
    ) -> Result<(), String> {
        match message {
            FromSandbox::Op { op, call, size } => self.open_op(op, call, size, told),
            FromSandbox::OpData { op } => self.op_data(op, &body, told),
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
                self.open_model(request, head, size, told)
            }
            FromSandbox::ModelData { request } => self.model_data(request, &body, told),
            FromSandbox::Done { stream } => self.route(io, stream, Reply::Done, told),
            FromSandbox::Failed { stream, message } => {
                self.route(io, stream, Reply::Failed(message), told)
            }
            FromSandbox::Exited { stream, status } => {
                self.route(io, stream, Reply::Exited(status), told)
            }
            FromSandbox::Entry { stream, entry } => {
                self.route(io, stream, Reply::Entry(entry), told)
            }
            FromSandbox::Data { stream } => self.route(io, stream, Reply::Data(body), told),
            FromSandbox::End { stream } => self.route(io, stream, Reply::End, told),
        }
    }
}

fn closed_by_sandbox(error: &io::Error) -> bool {
    // A sandbox that closes its end with what the gateway sent still unread resets the connection
    // rather than ends it cleanly, which a read then fails with, and a write once the reset has
    // come, as a broken pipe; which of these the gateway meets first is a race.
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
    )
}

fn on_received<C: Owner>(
    owner: &mut C,
    io: &mut Io<C>,
    token: u64,
    buffer: Vec<u8>,
    result: io::Result<usize>,
) {
    let id = SessionId::of(token);
    owner.sessions().with(id, |session, told| {
        session.received(io, buffer, result, told);
    });
    after(owner, io, id);
}

fn on_sent<C: Owner>(
    owner: &mut C,
    io: &mut Io<C>,
    token: u64,
    buffer: Vec<u8>,
    result: io::Result<usize>,
) {
    let id = SessionId::of(token);
    owner
        .sessions()
        .with(id, |session, told| session.sent(io, buffer, result, told));
    after(owner, io, id);
}

pub(crate) fn on_accepted<C: Owner>(
    owner: &mut C,
    io: &mut Io<C>,
    token: u64,
    accepted: io::Result<sio::Stream>,
) {
    let id = SessionId::of(token);
    owner.sessions().with(id, |session, told| {
        session.accepting = false;
        match accepted {
            Ok(stream) if session.ended().is_none() => session.accepted(io, stream),
            // A session given up on as its sandbox connected doesn't take the connection.
            Ok(stream) => io.close(stream),
            Err(error) => {
                if session.ended().is_none() {
                    let what = "a connection to the session's socket can't be accepted";
                    let error = crate::context(what, &error);
                    session.fail_accept(io, error, told);
                }
            }
        }
    });
    after(owner, io, id);
}

pub(crate) fn after<C: Owner>(owner: &mut C, io: &mut Io<C>, id: SessionId) {
    owner.sessions().reap(io, id);
    crate::Sessions::hand_over(owner, io);
}
