//! A gateway on a loop of its own, on a thread of the test's.
//!
//! A test drives each session that it accepts through a [`Session`], whose calls the loop asks of
//! the session as its owner, and hears the session's arrivals on a channel.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a test helper fails the test that calls it"
)]

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use gateway::{
    Answer, Arrival, Ask, IncomingOp, Met, Process, Put, RefusedOp, Registration, Response,
    SessionId, Sessions, Sockets, Takes,
};
use schema::ir::{AbsolutePath, Name};
use schema::sandbox::op::OpReply;
use schema::sandbox::tree::Limits;
use schema::sandbox::{Entry, Launch, Root, SocketDial};
use stdx::Deadline;
use stdx::io::{Backend, Events, Io, Loop};
use stdx::time::{Clock as _, Machine};

/// A loop on a thread of its own, which holds sessions that the test drives. Its deadlines are
/// times on its backend's clock, the machine's by default.
pub struct Gateway {
    events: Events<Event>,
    thread: Option<JoinHandle<()>>,
}

/// A session on a [`Gateway`]'s loop, which the test owns. Each call asks the loop for what the
/// session's owner asks, and waits for its answer. Dropping it drops the session.
pub struct Session {
    events: Events<Event>,
    id: SessionId,
}

/// A request asked of a [`Session`] whose answer is still to take.
pub struct Ticket {
    events: Events<Event>,
    id: SessionId,
    token: u64,
    told: Receiver<Told>,
}

enum Event {
    Issue {
        sockets: Arc<Sockets>,
        name: Name,
        reply: SyncSender<io::Result<(SocketDial, Registration)>>,
    },
    Accept {
        meeting: Meeting,
        by: Duration,
        takes: Takes,
        arrivals: Sender<Arrival>,
        reply: SyncSender<io::Result<SessionId>>,
    },
    Ask {
        id: SessionId,
        // `None` for the set-up of the sandbox ([`Sessions::set_up`]).
        ask: Option<Box<Ask>>,
        by: Duration,
        told: SyncSender<Told>,
    },
    Cancel {
        id: SessionId,
        token: u64,
        done: SyncSender<()>,
    },
    Drop(SessionId),
    Stop,
}

// What the loop tells a ticket: the token of its request, at once, then the answer.
enum Told {
    Token(u64),
    Answer(Answer),
}

enum Meeting {
    Socket(gateway::SocketMeeting),
    Met(Met),
}

struct Owner {
    sessions: Sessions,
    accepting: BTreeMap<u64, (Sender<Arrival>, SyncSender<io::Result<SessionId>>)>,
    arrivals: BTreeMap<SessionId, Sender<Arrival>>,
    waiting: BTreeMap<u64, SyncSender<Told>>,
    next: u64,
    stopped: bool,
}

impl Owner {
    fn token(&mut self) -> u64 {
        let token = self.next;
        self.next += 1;
        token
    }
}

impl Loop for Owner {
    type Event = Event;

    fn event(&mut self, io: &mut Io<Self>, event: Event) {
        match event {
            Event::Issue {
                sockets,
                name,
                reply,
            } => {
                let _ = reply.send(sockets.issue(io, &name));
            }
            Event::Accept {
                meeting,
                by,
                takes,
                arrivals,
                reply,
            } => {
                let token = self.token();
                self.accepting.insert(token, (arrivals, reply));
                match meeting {
                    Meeting::Socket(meeting) => {
                        Sessions::accept(self, io, meeting, (by, takes), token);
                    }
                    Meeting::Met(met) => {
                        Sessions::open(self, io, met, (by, takes), token);
                    }
                }
            }
            Event::Ask { id, ask, by, told } => {
                let token = self.token();
                // The ticket's channel holds both, and the answer never comes in the ask's call.
                let _ = told.send(Told::Token(token));
                self.waiting.insert(token, told);
                match ask {
                    Some(ask) => {
                        Sessions::ask(self, io, id, *ask, (by, token));
                    }
                    None => Sessions::set_up(self, io, id, (by, token)),
                }
            }
            Event::Cancel { id, token, done } => {
                Sessions::cancel(self, io, id, token);
                let _ = done.send(());
            }
            Event::Drop(id) => {
                Sessions::drop_session(self, io, id);
                // Its end has arrived, and nothing arrives after it.
                self.arrivals.remove(&id);
            }
            Event::Stop => self.stopped = true,
        }
    }
}

impl gateway::Owner for Owner {
    fn sessions(&mut self) -> &mut Sessions {
        &mut self.sessions
    }

    fn arrived(&mut self, _: &mut Io<Self>, id: SessionId, arrival: Arrival) {
        if let Some(arrivals) = self.arrivals.get(&id) {
            // The test may have stopped listening.
            let _ = arrivals.send(arrival);
        }
    }

    fn answered(&mut self, _: &mut Io<Self>, token: u64, answer: Answer) {
        if let Some((arrivals, reply)) = self.accepting.remove(&token) {
            let Answer::Accepted(accepted) = answer else {
                panic!("an accept was answered with {answer:?}")
            };
            if let Ok(id) = accepted {
                self.arrivals.insert(id, arrivals);
            }
            let _ = reply.send(accepted);
            return;
        }
        let told = (self.waiting.remove(&token))
            .unwrap_or_else(|| panic!("the test's loop asked nothing that {answer:?} answers"));
        // The ticket may have been dropped unanswered.
        let _ = told.send(Told::Answer(answer));
    }
}

/// What a session that takes no ops and no model requests takes.
pub fn passive() -> Takes {
    Takes {
        ops: None,
        models: false,
    }
}

impl Gateway {
    /// A loop on the machine's backend.
    pub fn new() -> Self {
        Self::on(stdx::io::machine().unwrap())
    }

    /// A loop on `backend`.
    pub fn on(backend: Box<dyn Backend>) -> Self {
        let io = Io::new(backend, 4096);
        let events = io.events();
        let thread = thread::Builder::new()
            .name("gateway".to_owned())
            .spawn(move || serve(io))
            .unwrap();
        Self {
            events,
            thread: Some(thread),
        }
    }

    /// Issues the socket of the sandbox `name` below `sockets`, on the loop.
    pub fn issue(
        &self,
        sockets: &Arc<Sockets>,
        name: &Name,
    ) -> io::Result<(SocketDial, Registration)> {
        let (reply, replied) = mpsc::sync_channel(1);
        self.events.send(Event::Issue {
            sockets: Arc::clone(sockets),
            name: name.clone(),
            reply,
        });
        replied.recv().unwrap()
    }

    /// Meets the sandbox that `registration` waits for by `by`, a time on the loop's clock, and
    /// opens its session, which takes what `takes` says, and sends its arrivals to `arrivals`.
    /// A meeting over the network waits on this thread, by the machine's clock.
    pub fn accept(
        &self,
        registration: Registration,
        by: Duration,
        takes: Takes,
        arrivals: Sender<Arrival>,
    ) -> io::Result<Session> {
        self.accept_stopped(registration, by, takes, arrivals, || false)
    }

    /// As [`Gateway::accept`], but that a meeting over the network asks `stopped` as it waits,
    /// and fails once it says yes.
    pub fn accept_stopped(
        &self,
        registration: Registration,
        by: Duration,
        takes: Takes,
        arrivals: Sender<Arrival>,
        stopped: impl Fn() -> bool,
    ) -> io::Result<Session> {
        let meeting = match registration {
            Registration::Socket(meeting) => Meeting::Socket(meeting),
            Registration::Elsewhere(elsewhere) => {
                let left = by.saturating_sub(Machine.now());
                Meeting::Met(elsewhere.meet(Deadline::after(left), stopped)?)
            }
        };
        let (reply, replied) = mpsc::sync_channel(1);
        self.events.send(Event::Accept {
            meeting,
            by,
            takes,
            arrivals,
            reply,
        });
        let id = replied.recv().unwrap()?;
        Ok(Session {
            events: self.events.clone(),
            id,
        })
    }
}

impl Default for Gateway {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.events.send(Event::Stop);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("the test's loop panicked");
        }
    }
}

fn serve(mut io: Io<Owner>) {
    let mut owner = Owner {
        sessions: Sessions::new(Duration::ZERO),
        accepting: BTreeMap::new(),
        arrivals: BTreeMap::new(),
        waiting: BTreeMap::new(),
        next: 0,
        stopped: false,
    };
    // It stops once the test has dropped it and its sessions have ended: the drop's event wakes it.
    while !owner.stopped || owner.sessions.busy() {
        let until = owner.sessions.due().unwrap_or(Duration::MAX);
        io.run_until(&mut owner, until);
        Sessions::tick(&mut owner, &mut io);
    }
}

impl Session {
    /// Asks the session for what `ask` says, by `by`, and returns at once, with the request's
    /// ticket.
    pub fn ask(&self, ask: Ask, by: Duration) -> Ticket {
        self.ticket(Some(Box::new(ask)), by)
    }

    fn ticket(&self, ask: Option<Box<Ask>>, by: Duration) -> Ticket {
        let (told, telling) = mpsc::sync_channel(2);
        self.events.send(Event::Ask {
            id: self.id,
            ask,
            by,
            told,
        });
        let Ok(Told::Token(token)) = telling.recv() else {
            panic!("the test's loop gave a request no token")
        };
        Ticket {
            events: self.events.clone(),
            id: self.id,
            token,
            told: telling,
        }
    }

    /// Puts what every sandbox needs before its first command ([`Sessions::set_up`]).
    pub fn set_up(&self, by: Duration) -> io::Result<()> {
        self.ticket(None, by).done()
    }

    /// Puts `entries` below `dir`, which it makes with its missing parents unless it exists.
    pub fn put(
        &self,
        dir: &AbsolutePath,
        entries: impl IntoIterator<Item = Put>,
        by: Duration,
    ) -> io::Result<()> {
        let ask = Ask::Put(dir.clone(), Root::Kept, entries.into_iter().collect());
        self.ask(ask, by).done()
    }

    pub fn freeze(&self, by: Duration) -> io::Result<()> {
        self.ask(Ask::Freeze, by).done()
    }

    /// Collects the tree at `path` into `into`, a new directory.
    pub fn collect(
        &self,
        path: &AbsolutePath,
        into: &Path,
        limits: Limits,
        by: Duration,
    ) -> io::Result<()> {
        let ask = Ask::Collect(path.clone(), into.to_owned(), limits);
        self.ask(ask, by).collected().map(drop)
    }

    pub fn start(&self, launch: Launch, by: Duration) -> io::Result<Process> {
        match self.ask(Ask::Start(launch), by).wait() {
            Answer::Started(started) => started,
            other => panic!("a start was answered with {other:?}"),
        }
    }

    pub fn kill(&self, process: Process, by: Duration) -> io::Result<()> {
        self.ask(Ask::Kill(process), by).done()
    }

    pub fn reply(&self, op: IncomingOp, reply: OpReply, by: Duration) -> io::Result<()> {
        self.ask(Ask::Reply(op, reply), by).done()
    }

    pub fn refuse(&self, refused: RefusedOp, by: Duration) -> io::Result<()> {
        self.ask(Ask::Refuse(refused), by).done()
    }

    /// Sends a frame of the response to the model request `number`, whose deadline is `by`.
    pub fn respond(&self, number: u32, response: Response, by: Duration) -> io::Result<()> {
        self.ask(Ask::Respond(number, response), by).done()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.events.send(Event::Drop(self.id));
    }
}

impl Ticket {
    /// Gives the request up, if it is a collect or a put under way ([`Sessions::cancel`]), and
    /// returns once the loop has.
    pub fn cancel(&self) {
        let (done, cancelled) = mpsc::sync_channel(1);
        self.events.send(Event::Cancel {
            id: self.id,
            token: self.token,
            done,
        });
        cancelled.recv().unwrap();
    }

    /// Waits for the request's answer.
    pub fn wait(self) -> Answer {
        match self.told.recv() {
            Ok(Told::Answer(answer)) => answer,
            Ok(Told::Token(token)) => panic!("a request was given a second token, {token}"),
            Err(mpsc::RecvError) => panic!("the test's loop dropped a request unanswered"),
        }
    }

    /// Waits for the answer of a request that [`Answer::Done`] answers.
    pub fn done(self) -> io::Result<()> {
        match self.wait() {
            Answer::Done(done) => done,
            other => panic!("a request that `done` answers was answered with {other:?}"),
        }
    }

    /// Waits for the answer of a collect.
    pub fn collected(self) -> io::Result<Vec<Entry>> {
        match self.wait() {
            Answer::Collected(collected) => collected,
            other => panic!("a collect was answered with {other:?}"),
        }
    }
}
