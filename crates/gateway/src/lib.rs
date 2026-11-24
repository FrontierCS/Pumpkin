//! The host's end of the sessions that sandboxes open, by [the protocol](schema::sandbox).
//!
//! The sessions of a process live on its loop ([`stdx::io`]), in its owner's [`Sessions`]. A
//! [`Registration`] meets one sandbox and opens its session there. The loop's owner drives a
//! session by [`Sessions::ask`], whose [`Answer`] it hears with the token it gave
//! ([`Owner::answered`]), and hears what the sandbox sends as [`Arrival`]s ([`Owner::arrived`]).
//! What an op or a model request means is the owner's to decide: the gateway authorizes nothing,
//! but bounds what they cost the host. The owner answers an op with [`Ask::Reply`] or
//! [`Ask::Refuse`], and a model request with [`Ask::Respond`], a frame of its response at a time.
//!
//! A sandbox, however buggy, can't harm the host through its session: a message that fails to
//! parse or comes out of turn, more replies than a waiting request takes, and a malformed tree
//! end the session, which stops the sandbox. A late reply, on a stream whose request has given
//! up, is dropped. A collected tree is written into a new directory of its own, within
//! [`Limits`](schema::sandbox::tree::Limits): a tree that passes them fails only its collect.
//!
//! # Deadlines
//!
//! Every request waits on the sandbox until its caller's deadline, a time since the [`Sessions`]'
//! origin on the loop's clock, so a slow link makes it wait, never fail. The loop's owner calls
//! [`Sessions::tick`] after each batch, and runs its next batch by [`Sessions::due`] at the latest.
//! A request whose deadline passes fails with [`io::ErrorKind::TimedOut`] and ends the session;
//! so does a sandbox that closes its connection, at once. A request takes an answer already read,
//! however late it looks. A frame may also wait behind another's, until that one's deadline.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::path::PathBuf;
use std::time::Duration;

pub use ask::{Ask, Response};
pub use meeting::{
    Dialer, Elsewhere, GatewayAddress, Listener, Met, Registration, SocketMeeting, Sockets,
    Unattested, Unreached,
};
pub use op::Op;
use schema::ir::{GpuModel, Mode};
use schema::sandbox::op::OpCall;
use schema::sandbox::{Entry, ExitStatus, OpPayloadLimit, RelativePath, pumpkin_dir};
use stdx::io::{Io, Loop};

mod ask;
mod bound;
mod frames;
mod meeting;
mod op;
mod session;
mod trees;

/// What owns a loop's sessions, and hears what they say, in the order it happened.
pub trait Owner: Loop {
    fn sessions(&mut self) -> &mut Sessions;

    /// Takes what the session `id` of the owner's handed it.
    fn arrived(&mut self, io: &mut Io<Self>, id: SessionId, arrival: Arrival);

    /// Takes the answer to the request that was given `token`.
    fn answered(&mut self, io: &mut Io<Self>, token: u64, answer: Answer);
}

// What a session tells its loop's owner, queued until the owner takes it.
enum Heard {
    Arrival(SessionId, Arrival),
    Answer(u64, Answer),
}

/// What a request came to.
#[derive(Debug)]
pub enum Answer {
    /// A put's, a freeze's, a kill's, or a reply's to an op.
    Done(io::Result<()>),
    /// A start's: the process whose [`Arrival::Exited`] follows.
    Started(io::Result<Process>),
    /// A collect's: the tree's entries, each with its mode as the sandbox sent it, which the
    /// host's umask may have narrowed in its directory.
    Collected(io::Result<Vec<Entry>>),
    /// An accept's: the session that it opened.
    Accepted(io::Result<SessionId>),
}

/// A session of a [`Sessions`], until the owner drops it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionId {
    index: u32,
    generation: u32,
}

impl SessionId {
    fn token(self) -> u64 {
        (u64::from(self.index) << 32) | u64::from(self.generation)
    }

    fn of(token: u64) -> Self {
        let [i0, i1, i2, i3, g0, g1, g2, g3] = token.to_be_bytes();
        Self {
            index: u32::from_be_bytes([i0, i1, i2, i3]),
            generation: u32::from_be_bytes([g0, g1, g2, g3]),
        }
    }
}

/// A command started on a session, which names it on that session alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Process {
    session: SessionId,
    stream: u32,
}

/// An entry of a tree to put.
#[derive(Debug)]
pub enum Put {
    Dir(RelativePath, Mode),
    File(RelativePath, Mode, Vec<u8>),
    /// The host's file at the path, opened as the put reaches it and read as it is sent, whose
    /// size is its size then: one that can't be opened, or then reads short, fails the put, and
    /// the session lives on.
    Read(RelativePath, Mode, PathBuf),
}

#[derive(Debug)]
#[must_use = "an op dropped unanswered wedges the sandbox's ops, which wait on it for good: \
              answer it with `Ask::Reply`"]
pub struct IncomingOp {
    session: SessionId,
    number: u32,
    pub op: Op,
}

/// What a session hands its owner, in the order the sandbox sent it, ending with the session's
/// end, once.
#[derive(Debug)]
#[must_use = "an arrival may hold an op, which dropped unanswered wedges the sandbox's ops, or a \
              model request, which dropped unanswered holds its client and its place in the \
              flight: answer each"]
pub enum Arrival {
    /// An op that arrived whole, for the owner to answer.
    Op(IncomingOp),
    /// An op whose payload was over the session's limit, for the owner to refuse.
    Refused(RefusedOp),
    /// A model request that arrived whole, for the owner to answer.
    ModelRequest(IncomingModelRequest),
    /// The exit of the command that a start returned `process` for.
    Exited {
        process: Process,
        status: ExitStatus,
    },
    /// The session's end, for `reason`.
    Ended { reason: String },
}

/// An op whose payload was over the session's limit, which all arrived and was dropped, in flight
/// until it is refused, on the session that handed it out.
#[derive(Debug)]
#[must_use = "an op dropped unanswered wedges the sandbox's ops, which wait on it for good: \
              answer it with `Ask::Refuse`"]
pub struct RefusedOp {
    session: SessionId,
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

/// A model request that arrived whole, in flight until its response ends ([`Ask::Respond`]), on
/// the session that handed it out.
#[derive(Debug)]
#[must_use = "a model request left unanswered holds its client, and its place in the flight, \
              until the session ends: answer it with `Ask::Respond`, ending with `Response::End`"]
pub struct IncomingModelRequest {
    pub request: ModelRequest,
    /// The session that it came on, which [`Ask::Respond`] answers it on, with its `number`.
    pub session: SessionId,
    pub number: u32,
}

/// An HTTP request to a sandbox's model endpoint, as its client sent it, whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRequest {
    pub method: String,
    /// The request's target: its path and any query, such as `/responses`.
    pub path: String,
    /// The headers, in the order the client sent them, names in lowercase.
    pub headers: Vec<(String, String)>,
    /// The body, of at most [`MAX_MODEL_REQUEST_BYTES`](schema::sandbox::MAX_MODEL_REQUEST_BYTES).
    pub body: Vec<u8>,
}

/// Where a request's answer goes: to the loop's owner, with its token.
#[derive(Clone, Copy)]
enum Answerer {
    Owner(u64),
    // The owner's set-up of the sandbox ([`Sessions::set_up`]), whose failure says what failed.
    SetUp(u64),
}

impl Answerer {
    fn token(&self) -> u64 {
        match self {
            Self::Owner(token) | Self::SetUp(token) => *token,
        }
    }
}

/// The sessions of a loop, which its owner holds.
pub struct Sessions {
    slots: Vec<SessionSlot>,
    free: Vec<u32>,
    // What the sessions told the owner that it hasn't taken yet.
    heard: VecDeque<Heard>,
    // Whether a `next_tick` that hands `heard` over is in flight.
    flushing: bool,
    origin: Duration,
}

struct SessionSlot {
    generation: u32,
    session: Option<session::Session>,
}

impl Sessions {
    /// Sessions whose deadlines are times since `origin` on the loop's clock.
    pub fn new(origin: Duration) -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            heard: VecDeque::new(),
            flushing: false,
            origin,
        }
    }

    /// The earliest deadline that a session waits on, as a time on the loop's clock, if any.
    pub fn due(&self) -> Option<Duration> {
        let due = self.live().filter_map(session::Session::due).min()?;
        Some(self.origin.saturating_add(due))
    }

    /// Ends each session whose request's deadline has passed by `now`, the time of the batch on
    /// the loop's clock, and hands the owner what that says.
    pub fn tick<C: Owner>(owner: &mut C, io: &mut Io<C>) {
        let now = io.now().saturating_sub(owner.sessions().origin);
        owner
            .sessions()
            .each(|session, told| session.tick(io, now, told));
        Self::hand_over(owner, io);
    }

    /// The GPUs that the sandbox's `sandboxd` saw as it started, each by its model as `nvidia-smi`
    /// names it, if it could tell ([`Hello::gpus`](schema::sandbox::Hello::gpus)).
    pub fn gpus(&self, id: SessionId) -> Option<&[GpuModel]> {
        let slot =
            (self.slots.get(id.index as usize)).filter(|slot| slot.generation == id.generation)?;
        slot.session.as_ref()?.gpus()
    }

    /// Ends the session, if it hasn't ended, and forgets it once nothing is in flight on it.
    pub fn drop_session<C: Owner>(owner: &mut C, io: &mut Io<C>, id: SessionId) {
        owner.sessions().with(id, |session, heard| {
            session.drop_by_owner(io, heard);
        });
        owner.sessions().reap(io, id);
        Self::hand_over(owner, io);
    }

    fn live(&self) -> impl Iterator<Item = &session::Session> {
        self.slots.iter().filter_map(|slot| slot.session.as_ref())
    }

    // Runs `act` on each session held, with where it tells what it heard.
    fn each(&mut self, mut act: impl FnMut(&mut session::Session, &mut Told<'_>)) {
        let Self { slots, heard, .. } = self;
        for (index, slot) in (0..).zip(slots) {
            let id = SessionId {
                index,
                generation: slot.generation,
            };
            if let Some(session) = &mut slot.session {
                act(
                    session,
                    &mut Told {
                        id,
                        heard: &mut *heard,
                    },
                );
            }
        }
    }

    // The session, if it is still held, and where it tells what it heard.
    fn held(&mut self, id: SessionId) -> (Option<&mut session::Session>, Told<'_>) {
        let Self { slots, heard, .. } = self;
        let session = (slots.get_mut(id.index as usize))
            .filter(|slot| slot.generation == id.generation)
            .and_then(|slot| slot.session.as_mut());
        (session, Told { id, heard })
    }

    // Runs `act` on the session, if it is still held, with where it tells what it heard.
    fn with<T>(
        &mut self,
        id: SessionId,
        act: impl FnOnce(&mut session::Session, &mut Told<'_>) -> T,
    ) -> Option<T> {
        let (session, mut told) = self.held(id);
        session.map(|session| act(session, &mut told))
    }

    // Opens a session in a slot of its own, in the phase that `phase` makes for its id, taking
    // what `takes` says.
    fn open_slot(
        &mut self,
        phase: impl FnOnce(SessionId) -> session::Phase,
        takes: Takes,
    ) -> SessionId {
        let index = self.free.pop().unwrap_or_else(|| {
            let Ok(index) = u32::try_from(self.slots.len()) else {
                unreachable!("a loop holds more sessions than a u32 counts")
            };
            self.slots.push(SessionSlot {
                generation: 0,
                session: None,
            });
            index
        });
        let slot = &mut self.slots[index as usize];
        let id = SessionId {
            index,
            generation: slot.generation,
        };
        let session = session::Session::new(id, self.origin, phase(id), takes);
        slot.session = Some(session);
        id
    }

    // Forgets the session once its owner has dropped it and nothing is in flight on it.
    fn reap<C: Owner>(&mut self, io: &mut Io<C>, id: SessionId) {
        let Some(slot) = self.slots.get_mut(id.index as usize) else {
            return;
        };
        if slot.generation != id.generation {
            return;
        }
        if slot.session.as_ref().is_some_and(session::Session::done) {
            if let Some(session) = slot.session.take() {
                session.close(io);
            }
            slot.generation = slot.generation.wrapping_add(1);
            self.free.push(id.index);
        }
    }

    // Hands the owner what the sessions told it, in order.
    fn hand_over<C: Owner>(owner: &mut C, io: &mut Io<C>) {
        while let Some(heard) = owner.sessions().heard.pop_front() {
            match heard {
                Heard::Arrival(id, arrival) => owner.arrived(io, id, arrival),
                Heard::Answer(token, answer) => owner.answered(io, token, answer),
            }
        }
    }

    // Hands the owner what the sessions told it in the next batch, as an answer never comes in the
    // call that asked for it.
    fn flush_later<C: Owner>(&mut self, io: &mut Io<C>) {
        if self.flushing || self.heard.is_empty() {
            return;
        }
        self.flushing = true;
        io.next_tick(0, |owner: &mut C, io, _| {
            owner.sessions().flushing = false;
            Self::hand_over(owner, io);
        });
    }
}

// Where a session tells what it heard: to the owner's queue.
struct Told<'a> {
    id: SessionId,
    heard: &'a mut VecDeque<Heard>,
}

impl Told<'_> {
    fn answer(&mut self, answerer: Answerer, answer: Answer) {
        match answerer {
            Answerer::Owner(token) => self.heard.push_back(Heard::Answer(token, answer)),
            Answerer::SetUp(token) => {
                let answer = match answer {
                    Answer::Done(Err(error)) => {
                        let failed = ask::set_up_failed(&pumpkin_dir(), &error);
                        Answer::Done(Err(io::Error::new(error.kind(), failed)))
                    }
                    answer @ (Answer::Done(Ok(()))
                    | Answer::Started(_)
                    | Answer::Collected(_)
                    | Answer::Accepted(_)) => answer,
                };
                self.heard.push_back(Heard::Answer(token, answer));
            }
        }
    }

    fn arrived(&mut self, arrival: Arrival) {
        self.heard.push_back(Heard::Arrival(self.id, arrival));
    }
}

/// A request's deadline, as a time since the sessions' origin, and whose it is, which the reason
/// for the session's end names if it passes: a request's, which the owner's reply to an op shares,
/// or a response's.
#[derive(Clone, Copy, Debug)]
struct By {
    at: Duration,
    due: Due,
}

impl By {
    fn request(at: Duration) -> Self {
        Self {
            at,
            due: Due::Request,
        }
    }
}

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

/// What a session's owner gives as it opens it: the limit on an op's payload, none for a session
/// that takes no ops, and whether it takes model requests.
#[derive(Clone, Copy, Debug)]
pub struct Takes {
    pub ops: Option<OpPayloadLimit>,
    pub models: bool,
}

impl Sessions {
    /// Waits on the loop for the sandbox to connect to `meeting`'s socket by `by`, reads its hello
    /// and opens its session. The owner hears [`Answer::Accepted`] with `token`. Returns the
    /// session, which its owner may drop before it opens. A sandbox that has not met the gateway
    /// by `by` fails it with `TimedOut`, and a hello that is wrong, which the sandbox is told of,
    /// with `InvalidData`.
    pub fn accept<C: Owner>(
        owner: &mut C,
        io: &mut Io<C>,
        meeting: SocketMeeting,
        (by, takes): (Duration, Takes),
        token: u64,
    ) -> SessionId {
        let phase = |id: SessionId| {
            io.accept(&meeting.listener, id.token(), session::on_accepted::<C>);
            session::Phase::Accepting {
                listener: Some(meeting),
                by,
                answer: Some(Answerer::Owner(token)),
            }
        };
        owner.sessions().open_slot(phase, takes)
    }

    /// Opens the session of a sandbox that met the gateway on another thread, answering its
    /// hello by `by`. The owner hears [`Answer::Accepted`] with `token`.
    pub fn open<C: Owner>(
        owner: &mut C,
        io: &mut Io<C>,
        met: Met,
        (by, takes): (Duration, Takes),
        token: u64,
    ) -> SessionId {
        let Met {
            connection,
            gpus,
            grant,
        } = met;
        let phase = |_| session::Phase::Welcoming {
            by,
            answer: Some(Answerer::Owner(token)),
        };
        let id = owner.sessions().open_slot(phase, takes);
        let parts = connection
            .into_parts()
            .and_then(|(fd, kept)| Ok((io.adopt(fd)?, kept)));
        owner.sessions().with(id, |session, told| match parts {
            Ok((stream, kept)) => {
                session.stream(stream);
                session.kept = kept;
                session.set_gpus(gpus);
                session.welcome(io, grant, told);
            }
            Err(error) => {
                let error = context("the sandbox's connection can't be held", &error);
                session.fail_accept(io, error, told);
            }
        });
        owner.sessions().reap(io, id);
        owner.sessions().flush_later(io);
        id
    }

    /// Gives up the collect or the put that the owner asked of the session with `token`, if it
    /// is under way: either fails as interrupted, the put once it has ended its tree with the
    /// entries sent so far, unless it had sent them all. The session stays open. The owner hears
    /// the answer once this call has returned, never in it.
    pub fn cancel<C: Owner>(owner: &mut C, io: &mut Io<C>, id: SessionId, token: u64) {
        owner
            .sessions()
            .with(id, |session, told| session.cancel(io, token, told));
        owner.sessions().flush_later(io);
    }

    /// Whether it still holds a session: one that its owner hasn't dropped, or that something is
    /// still in flight on.
    pub fn busy(&self) -> bool {
        self.live().next().is_some()
    }
}

fn context(what: impl std::fmt::Display, error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{what}: {error}"))
}

fn broke(what: &str) -> String {
    format!("the sandbox broke the protocol: {what}")
}

const SANDBOX_ENDED: &str = "the sandbox ended the session";

// The model requests and ops a session holds in flight.
#[derive(Debug, Default)]
struct Flight {
    // The op in flight: a session has one at most.
    op: Option<u32>,
    models: BTreeSet<u32>,
    // The model requests whose bodies are arriving, by number, each with the bytes still due.
    bodies: BTreeMap<u32, (ModelRequest, u64)>,
}
