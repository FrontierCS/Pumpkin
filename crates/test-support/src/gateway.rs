//! A gateway on a loop of its own, on a thread of the test's: a test drives its sessions as a
//! run's thread drives one, through a [`Remote`] whose calls the loop serves, and hears their
//! arrivals on a channel.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a test helper fails the test that calls it"
)]

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::sync::mpsc::{self, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use gateway::{
    Answer, Arrival, Met, Registration, Remote, Request, SessionId, Sessions, Sockets, Takes,
};
use schema::ir::Name;
use schema::sandbox::SocketDial;
use stdx::Deadline;
use stdx::io::{Backend, Events, Inbox, Io, Loop};
use stdx::time::{Clock as _, Machine};

// How long the loop waits at most before it looks whether it should stop.
const LOOK: Duration = Duration::from_millis(10);

/// A loop on a thread of its own, which holds sessions that the test drives. Its deadlines are
/// times on its backend's clock, the machine's by default.
pub struct Gateway {
    events: Events<Ask>,
    thread: Option<JoinHandle<()>>,
}

enum Ask {
    Session(Box<Request>),
    Issue {
        sockets: Arc<Sockets>,
        name: Name,
        reply: SyncSender<io::Result<(SocketDial, Registration)>>,
    },
    Accept {
        meeting: Meeting,
        by: Duration,
        takes: Takes,
        arrivals: Box<dyn Fn(Arrival) + Send>,
        reply: SyncSender<io::Result<Remote>>,
    },
    Stop,
}

enum Meeting {
    Socket(gateway::SocketMeeting),
    Met(Met),
}

struct Owner {
    sessions: Sessions,
    accepting: BTreeMap<u64, Accepting>,
    next: u64,
    stopped: bool,
}

type Accepting = (Box<dyn Fn(Arrival) + Send>, SyncSender<io::Result<Remote>>);

impl Loop for Owner {
    type Event = Ask;

    fn event(&mut self, io: &mut Io<Self>, ask: Ask) {
        match ask {
            Ask::Session(request) => Sessions::serve(self, io, *request),
            Ask::Issue {
                sockets,
                name,
                reply,
            } => {
                let _ = reply.send(sockets.issue(io, &name));
            }
            Ask::Accept {
                meeting,
                by,
                takes,
                arrivals,
                reply,
            } => {
                let token = self.next;
                self.next += 1;
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
            Ask::Stop => self.stopped = true,
        }
    }
}

impl gateway::Owner for Owner {
    fn sessions(&mut self) -> &mut Sessions {
        &mut self.sessions
    }

    fn arrived(&mut self, _: &mut Io<Self>, _: SessionId, arrival: Arrival) {
        panic!("a session of the test's loop handed it {arrival:?}")
    }

    fn answered(&mut self, _: &mut Io<Self>, token: u64, answer: Answer) {
        let Answer::Accepted(accepted) = answer else {
            panic!("the test's loop asked nothing that {answer:?} answers")
        };
        let (arrivals, reply) = self.accepting.remove(&token).unwrap();
        let remote = accepted.map(|id| self.sessions.remote(id, arrivals));
        let _ = reply.send(remote);
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
        let (events, inbox) = Inbox::new(&*backend);
        let send = events.clone();
        let thread = thread::Builder::new()
            .name("gateway".to_owned())
            .spawn(move || serve(backend, inbox, send))
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
        self.events.send(Ask::Issue {
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
    ) -> io::Result<Remote> {
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
    ) -> io::Result<Remote> {
        let meeting = match registration {
            Registration::Socket(meeting) => Meeting::Socket(meeting),
            Registration::Elsewhere(elsewhere) => {
                let left = by.saturating_sub(Machine.now());
                Meeting::Met(elsewhere.meet(Deadline::after(left), stopped)?)
            }
        };
        let (reply, replied) = mpsc::sync_channel(1);
        let arrivals = Box::new(move |arrival| {
            let _ = arrivals.send(arrival);
        });
        self.events.send(Ask::Accept {
            meeting,
            by,
            takes,
            arrivals,
            reply,
        });
        replied.recv().unwrap()
    }
}

impl Default for Gateway {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.events.send(Ask::Stop);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("the test's loop panicked");
        }
    }
}

fn serve(backend: Box<dyn Backend>, inbox: Inbox<Ask>, send: Events<Ask>) {
    let mut io = Io::new(backend, 4096, inbox);
    let remote = Arc::new(move |request| send.send(Ask::Session(Box::new(request))));
    let mut owner = Owner {
        sessions: Sessions::new(Duration::ZERO, remote),
        accepting: BTreeMap::new(),
        next: 0,
        stopped: false,
    };
    // It stops once the test has dropped it and its sessions have ended.
    while !owner.stopped || owner.sessions.busy() {
        let look = io.now().saturating_add(LOOK);
        let until = owner.sessions.due().map_or(look, |due| due.min(look));
        io.run_until(&mut owner, until);
        Sessions::tick(&mut owner, &mut io);
    }
}
