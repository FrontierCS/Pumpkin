//! Every sandbox's start, delete and disposal, on the driver's loop. A provider's call runs on a
//! thread of its own, which completes into the loop as an [`Input`]; the record of each sandbox
//! lasts through the loop's [`Disk`]; and the session opens on the loop. What waits on each is the
//! agent's own steps, or a child's thread, which a channel of its own answers.

use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::time::{Duration, SystemTime};

use gateway::{Answer, Arrival, Elsewhere, Met, Registration, SessionId, Sessions, Takes};
use provider::{Create, Created, SandboxId, Undeleted, UnixTime};
use schema::ir::Name;
use schema::sandbox::OpPayloadLimit;
use stdx::{Context, Deadline};

use crate::disk::{Disk, Job};
use crate::host::Host;
use crate::log::Entry;
use crate::sandbox::{Made, Meets, Started, Unstarted, Which, provider_role};
use crate::{Input, Io, Runtime, disk, interruption, record};

/// A thread's ask for a sandbox's start.
pub(crate) struct Asked {
    pub(crate) which: Which,
    pub(crate) ops: Option<OpPayloadLimit>,
    pub(crate) by: Duration,
    pub(crate) arrivals: Box<dyn Fn(Arrival) + Send>,
    pub(crate) reply: SyncSender<Result<Started, Unstarted>>,
}

/// A thread's ask for the delete of its sandbox.
pub(crate) struct Delete {
    pub(crate) which: Which,
    pub(crate) id: SandboxId,
    pub(crate) reply: SyncSender<(Result<(), String>, stdx::io::Edge<Input>)>,
}

/// A thread's ask for the disposal of a sandbox that didn't start.
pub(crate) struct Dispose {
    pub(crate) which: Which,
    pub(crate) id: SandboxId,
    pub(crate) reason: String,
    pub(crate) reply: SyncSender<Result<String, String>>,
}

/// What a provider's thread completes into the loop with.
pub(crate) enum Edge {
    // A create that failed gives back the socket it was to be met on, for the loop to withdraw.
    Created(
        u64,
        Result<(Created, Meeting), (String, Option<gateway::SocketMeeting>)>,
    ),
    // Whether the provider lists the sandbox as ended, as a start waits for it to meet the gateway.
    Listed(u64, Result<bool, String>),
    // A meeting over the network, which a thread waited for.
    Met(u64, std::io::Result<Met>),
    // A delete, with what a disposal's message says of the sandbox's console.
    Deleted(u64, Option<Vec<String>>, Result<(), Undeleted>),
}

/// How a created sandbox meets the gateway: on the loop, or over the network, which a thread of
/// its own waits for once the sandbox is recorded.
pub(crate) enum Meeting {
    Socket(gateway::SocketMeeting),
    Elsewhere(Elsewhere),
}

// Why a start that a signal stopped as it waited for its sandbox failed.
fn stopped(socket: Option<&std::path::Path>) -> String {
    match socket {
        Some(path) => format!(
            "the accept was stopped before a sandbox connected to {}: expected its sandbox to \
             connect before the stop",
            path.display()
        ),
        None => "the accept was stopped before the gateway met the sandbox: expected the sandbox \
                 to meet it before the stop"
            .to_owned(),
    }
}

const ENDED: &str =
    "it ended before it met the gateway: expected its `sandboxd` to run until it connects";

pub(crate) fn loop_gone() -> ! {
    unreachable!(
        "the loop dropped a thread's ask unanswered, though the loop runs until every thread of the \
         run has ended, answering each ask"
    )
}

/// A start under way.
pub(crate) struct Starting {
    which: Which,
    by: Duration,
    takes: Takes,
    waiter: StartWaiter,
    step: Step,
    id: Option<SandboxId>,
    session: Option<SessionId>,
    meeting: Option<Meeting>,
    looks: Looks,
    // Where the sandbox connects, for a socket's start: what a stop says it waited for.
    socket: Option<std::path::PathBuf>,
    // Whether the loop has given up the meeting that a thread waits for over the network, which
    // the thread asks as it waits: shared with it.
    given_up: Option<Arc<AtomicBool>>,
}

/// Whether a sandbox has ended, as its provider lists it while its start waits for it to meet the
/// gateway: at most once per a fortieth of the run's quickest startup, from its create's return.
#[derive(Default)]
struct Looks {
    last: Duration,
    looking: bool,
    ended: bool,
    // The failure of the last look, if no look has succeeded since.
    failed: Option<String>,
}

pub(crate) enum StartWaiter {
    Agent(schema::workflow::AgentId),
    Thread(
        Box<dyn Fn(Arrival) + Send>,
        SyncSender<Result<Started, Unstarted>>,
    ),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Creating,
    Recording,
    Meeting,
    // The GPUs that the sandbox saw recorded, before its set-up.
    RecordingGpus,
    SettingUp,
}

/// A delete under way.
pub(crate) struct Deleting {
    which: Which,
    id: SandboxId,
    waiter: DeleteWaiter,
    recording: usize,
    failure: Option<String>,
    deleted: Option<Result<(), String>>,
    // The delete's log entries, written once its record is.
    logged: Vec<Entry>,
}

pub(crate) enum DeleteWaiter {
    Agent(schema::workflow::AgentId, Option<SessionId>),
    // A thread, which reports what follows the delete through the edge it is given, as the loop
    // waits for it.
    Thread(SyncSender<(Result<(), String>, stdx::io::Edge<Input>)>),
    Disposal(Box<Disposing>),
}

/// A disposal under way: the parts of its message, and who waits on it.
pub(crate) struct Disposing {
    parts: Vec<String>,
    waiter: DisposeWaiter,
}

pub(crate) enum DisposeWaiter {
    Agent(schema::workflow::AgentId),
    Thread(SyncSender<Result<String, String>>),
}

impl<'r, 's, H: Host + 'r> Runtime<'r, 's, H> {
    /// Starts `which`'s sandbox, which takes what `takes` says, by `by`, a time since the run
    /// started, and sets it up; `waiter` hears how it went.
    pub(crate) fn begin_start(
        &mut self,
        io: &mut Io<'r, 's, H>,
        which: Which,
        (by, takes): (Duration, Takes),
        waiter: StartWaiter,
    ) {
        let token = self.new_token();
        let starting = Starting {
            which,
            by,
            takes,
            waiter,
            step: Step::Creating,
            id: None,
            session: None,
            meeting: None,
            looks: Looks::default(),
            socket: None,
            given_up: None,
        };
        self.starts.insert(token, starting);
        let created = self.create(io, token, which, by);
        if let Err(reason) = created {
            self.start_failed(io, token, Unstarted::Uncreated(reason));
        }
    }

    // Asks the provider for the sandbox, on a thread of its own.
    #[expect(
        clippy::too_many_lines,
        reason = "most of it is the provider's thread, one arm per way to meet the gateway"
    )]
    fn create(
        &mut self,
        io: &mut Io<'r, 's, H>,
        token: u64,
        which: Which,
        by: Duration,
    ) -> Result<(), String> {
        let sandboxes = self.sandboxes;
        let workflow = sandboxes.workflow();
        let now = UnixTime::of(SystemTime::now())?;
        let left = by.saturating_sub(self.now(io));
        // The start's deadline by the machine's clock, which the provider reads, at most the hard
        // end, as the run is abandoned then.
        let by_unix = now.saturating_add(left).min(self.unix_hard_end);
        let role = which.role();
        let model = workflow.model(role).is_some();
        let placement = sandboxes.placed.placement(role);
        let hosting = sandboxes.providers.get(&placement.provider);
        let ops = self
            .starts
            .get(&token)
            .is_some_and(|start| start.takes.ops.is_some());
        let create = Create {
            run: sandboxes.run.clone(),
            name: which.name(),
            role: provider_role(workflow, role, sandboxes.capacity.instances(role)),
            instance: which.instance(),
            class: placement.class.clone(),
            image: sandboxes.images.get(role).clone(),
            sandboxd: hosting.sandboxd.clone(),
            ops,
            model,
            hard_end: self.unix_hard_end,
            by: by_unix,
        };
        let deadline = Deadline::after(left);
        // A socket's listener is the loop's, made before the create, which names its path.
        let socket = match hosting.meets {
            Meets::Sockets(_) => {
                let issued =
                    (sandboxes.sockets.issue(io, &create.name)).map_err(|e| e.to_string())?;
                Some(issued)
            }
            Meets::Private(..) | Meets::Dials(..) => None,
        };
        if let (Some((dial, _)), Some(start)) = (&socket, self.starts.get_mut(&token)) {
            start.socket = Some(dial.dir.join(schema::sandbox::SOCKET_NAME));
        }
        let meets = hosting.meets;
        let edge = io.edge();
        // A socket that a failed spawn drops stays open until the process ends: the kernel had no
        // thread to give, so the run is unlikely to go on long.
        self.spawn((&format!("create-{}", which.name()), None), move || {
            let created = match (meets, socket) {
                (Meets::Sockets(creator), Some((dial, Registration::Socket(meeting)))) => {
                    match creator.create(&create, dial) {
                        Ok(created) => Ok((created, Meeting::Socket(meeting))),
                        Err(error) => Err((error, Some(meeting))),
                    }
                }
                (Meets::Private(creator, listener), _) => {
                    let (dial, unattested) = listener.issue(deadline);
                    let created = creator.create(&create, dial).map(|(created, admits)| {
                        let elsewhere = elsewhere(unattested.admitting(admits));
                        (created, Meeting::Elsewhere(elsewhere))
                    });
                    created.map_err(|error| (error, None))
                }
                (Meets::Dials(creator, dialer), _) => dialer
                    .issue()
                    .map_err(|error| error.to_string())
                    .and_then(|(dial, unreached)| {
                        let (created, endpoint) = creator.create(&create, dial)?;
                        let elsewhere = elsewhere(unreached.at(endpoint));
                        Ok((created, Meeting::Elsewhere(elsewhere)))
                    })
                    .map_err(|error| (error, None)),
                (Meets::Sockets(_), _) => unreachable!(
                    "a socket's provider has no listener of the loop's, though the loop makes one \
                     before each of its creates"
                ),
            };
            edge.send(Input::Edge(Edge::Created(token, created)));
        })
    }

    pub(crate) fn edge(&mut self, io: &mut Io<'r, 's, H>, edge: Edge) {
        match edge {
            Edge::Created(token, created) => self.created(io, token, created),
            Edge::Listed(token, listed) => self.listed(io, token, listed),
            Edge::Met(token, met) => self.met(io, token, met),
            Edge::Deleted(token, parts, deleted) => self.deleted(io, token, parts, &deleted),
        }
    }

    fn created(
        &mut self,
        io: &mut Io<'r, 's, H>,
        token: u64,
        created: Result<(Created, Meeting), (String, Option<gateway::SocketMeeting>)>,
    ) {
        let Some(start) = self.starts.get_mut(&token) else {
            unreachable!("a create completed for no start, though each start waits for its create")
        };
        let which = start.which;
        let on = &self.sandboxes.placed.placement(which.role()).provider;
        let (Created { id }, meeting) = match created {
            Ok(created) => created,
            Err((reason, socket)) => {
                if let Some(socket) = socket {
                    socket.withdraw(io);
                }
                self.inventory.unnamed.insert(on);
                return self.start_failed(io, token, Unstarted::Uncreated(reason));
            }
        };
        start.id = Some(id.clone());
        start.meeting = Some(meeting);
        start.step = Step::Recording;
        // The first look comes a look's interval after the create returned, so every list speaks
        // for the sandbox it made.
        start.looks.last = self.at;
        let made = Made {
            on,
            id,
            which,
            deleted: false,
            gpus: None,
        };
        self.inventory.created.push(made.clone());
        self.record_sandbox(io, &made, None, token);
    }

    /// Writes `made`'s `sandbox.json`, whose end goes to `token`.
    pub(crate) fn record_sandbox(
        &mut self,
        io: &mut Io<'r, 's, H>,
        made: &Made<'r>,
        deleted: Option<Result<(), &str>>,
        token: u64,
    ) {
        let dir = self.dir.join(made.which.record_dir(self.workflow()));
        let bytes = record::sandbox_json(made.on, &made.id, made.gpus.as_deref(), deleted);
        if let Err(error) = fs::create_dir_all(&dir) {
            let error = format!("{}: {error}", dir.display());
            return disk::Owner::written(self, io, token, Err(error));
        }
        let name = "sandbox.json".to_owned();
        Disk::push(self, io, Job::Replace { dir, name, bytes }, token);
    }

    /// Takes the end of a write of a start's record.
    pub(crate) fn start_written(
        &mut self,
        io: &mut Io<'r, 's, H>,
        token: u64,
        written: Result<(), String>,
    ) {
        let Some(start) = self.starts.get_mut(&token) else {
            unreachable!("a start's record was written for no start")
        };
        let which = start.which;
        let id = start.id.clone();
        if let Err(reason) = written {
            let Some(id) = id else {
                unreachable!("a start records its sandbox only once it is created")
            };
            return self.start_failed(io, token, Unstarted::Created { which, id, reason });
        }
        if start.step == Step::RecordingGpus {
            return self.set_up(io, token);
        }
        // A start interrupted as it records meets no sandbox.
        if interruption::signal().is_some() {
            let Some(id) = id else {
                unreachable!("a start records its sandbox only once it is created")
            };
            let reason = stopped(start.socket.as_deref());
            return self.start_failed(io, token, Unstarted::Created { which, id, reason });
        }
        start.step = Step::Meeting;
        let (by, takes) = (start.by, start.takes);
        match start.meeting.take() {
            Some(Meeting::Socket(meeting)) => {
                let session = Sessions::accept(self, io, meeting, (by, takes), token);
                if let Some(start) = self.starts.get_mut(&token) {
                    start.session = Some(session);
                }
            }
            // The loop looks whether the sandbox has ended while a thread waits for it.
            Some(Meeting::Elsewhere(elsewhere)) => {
                let given_up = Arc::new(AtomicBool::new(false));
                start.given_up = Some(Arc::clone(&given_up));
                let deadline = Deadline::after(by.saturating_sub(self.at));
                let edge = io.edge();
                let spawned = self.spawn((&format!("meet-{}", which.name()), None), move || {
                    let stopped =
                        || interruption::signal().is_some() || given_up.load(Ordering::Relaxed);
                    let met = elsewhere.meet(deadline, stopped);
                    edge.send(Input::Edge(Edge::Met(token, met)));
                });
                if let (Err(reason), Some(id)) = (spawned, id) {
                    self.start_failed(io, token, Unstarted::Created { which, id, reason });
                }
            }
            None => unreachable!("a start's meeting is taken once, as it is recorded"),
        }
    }

    // Opens the session of a sandbox that met the gateway over the network.
    fn met(&mut self, io: &mut Io<'r, 's, H>, token: u64, met: std::io::Result<Met>) {
        // A start that a look or a signal ended needs its meeting no more, which drops.
        let Some(start) = self.starts.get(&token) else {
            return;
        };
        match met {
            Ok(met) => {
                let (by, takes) = (start.by, start.takes);
                let session = Sessions::open(self, io, met, (by, takes), token);
                if let Some(start) = self.starts.get_mut(&token) {
                    start.session = Some(session);
                }
            }
            Err(error) => self.start_answered(io, token, Answer::Accepted(Err(error))),
        }
    }

    /// Takes the answer to a start's step: its accept, or a put.
    pub(crate) fn start_answered(&mut self, io: &mut Io<'r, 's, H>, token: u64, answer: Answer) {
        // A start that a look ended hears its accept's end too.
        let Some(start) = self.starts.get_mut(&token) else {
            return;
        };
        let which = start.which;
        let id = start.id.clone().unwrap_or_else(|| {
            unreachable!("a start asks its session for anything only once its sandbox is created")
        });
        let failed = |reason: String| Unstarted::Created {
            which,
            id: id.clone(),
            reason,
        };
        match answer {
            Answer::Accepted(Ok(session)) => {
                start.session = Some(session);
                match self.sessions.gpus(session).map(<[_]>::to_vec) {
                    Some(gpus) => {
                        start.step = Step::RecordingGpus;
                        self.saw(io, token, which, &id, gpus);
                    }
                    None => self.set_up(io, token),
                }
            }
            Answer::Accepted(Err(error)) => {
                let reason = match (start.looks.ended, start.looks.failed.take()) {
                    (true, _) => ENDED.to_owned(),
                    (false, Some(failed)) if error.kind() == std::io::ErrorKind::TimedOut => {
                        format!("{error}; the last list of its provider failed: {failed}")
                    }
                    (false, _) => error.to_string(),
                };
                self.start_failed(io, token, failed(reason));
            }
            Answer::Done(Ok(())) => self.start_done(io, token),
            Answer::Done(Err(error)) => self.start_failed(io, token, failed(error.to_string())),
            other => unreachable!("{other:?} answered a start's step"),
        }
    }

    /// Ends the wait of each start whose sandbox is to meet the gateway, as a signal has come: the
    /// start fails, and what it made is disposed of.
    pub(crate) fn stop_meetings(&mut self, io: &mut Io<'r, 's, H>) {
        let meeting: Vec<u64> = (self.starts.iter())
            .filter(|(_, start)| start.step == Step::Meeting)
            .map(|(token, _)| *token)
            .collect();
        for token in meeting {
            let Some(start) = self.starts.get(&token) else {
                continue;
            };
            let (which, Some(id)) = (start.which, start.id.clone()) else {
                continue;
            };
            let reason = stopped(start.socket.as_deref());
            self.start_failed(io, token, Unstarted::Created { which, id, reason });
        }
    }

    /// The earliest time that a start's next look is due at, as a time since the run started.
    pub(crate) fn looks_due(&self) -> Option<Duration> {
        let between = self.sandboxes.between_looks;
        (self.starts.values())
            .filter(|start| start.step == Step::Meeting && !start.looks.looking)
            .filter(|start| !start.looks.ended)
            .map(|start| start.looks.last.saturating_add(between))
            .min()
    }

    /// Looks whether each start's sandbox that waits to meet the gateway has ended, where a look
    /// is due.
    pub(crate) fn look(&mut self, io: &mut Io<'r, 's, H>) {
        let (at, between) = (self.at, self.sandboxes.between_looks);
        let due: Vec<u64> = (self.starts.iter())
            .filter(|(_, start)| start.step == Step::Meeting && !start.looks.looking)
            .filter(|(_, start)| {
                !start.looks.ended && at >= start.looks.last.saturating_add(between)
            })
            .map(|(token, _)| *token)
            .collect();
        for token in due {
            let Some(start) = self.starts.get_mut(&token) else {
                continue;
            };
            start.looks.looking = true;
            let (Some(id), which) = (start.id.clone(), start.which) else {
                continue;
            };
            let provider = self.provider(which);
            let run = self.sandboxes.run.clone();
            let edge = io.edge();
            let spawned = self.spawn((&format!("look-{}", which.name()), None), move || {
                // A list leaves a sandbox out only once it is deleted, or has ended and been freed
                // (`Sweep::list`), so only a list that finds it running says it hasn't ended.
                let listed = provider
                    .list(&run)
                    .map(|listed| !(listed.iter()).any(|listed| listed.id == id && !listed.ended));
                edge.send(Input::Edge(Edge::Listed(token, listed)));
            });
            // A look that didn't start is a look that failed.
            if let Err(error) = spawned {
                self.listed(io, token, Err(error));
            }
        }
    }

    fn listed(&mut self, io: &mut Io<'r, 's, H>, token: u64, listed: Result<bool, String>) {
        let Some(start) = self.starts.get_mut(&token) else {
            // A start that ended as its look listed needs it no more.
            return;
        };
        start.looks.looking = false;
        start.looks.last = self.at;
        match listed {
            Ok(ended) => {
                start.looks.failed = None;
                start.looks.ended = ended;
            }
            Err(error) => start.looks.failed = Some(error),
        }
        // The wait ends at once for a sandbox that its provider lists as ended, which will never
        // connect.
        if start.looks.ended
            && start.step == Step::Meeting
            && let Some(id) = start.id.clone()
        {
            let which = start.which;
            let unstarted = Unstarted::Created {
                which,
                id,
                reason: ENDED.to_owned(),
            };
            self.start_failed(io, token, unstarted);
        }
    }

    // Records the GPUs that the sandbox's `sandboxd` saw.
    fn saw(
        &mut self,
        io: &mut Io<'r, 's, H>,
        token: u64,
        which: Which,
        id: &SandboxId,
        gpus: Vec<schema::ir::GpuModel>,
    ) {
        let made = (self.inventory.created.iter_mut())
            .filter(|made| made.id == *id)
            .map(|made| {
                made.gpus = Some(gpus.clone());
                made.clone()
            })
            .collect::<Vec<_>>();
        self.logged(Entry::Gpus(which.name(), gpus));
        for made in made {
            self.record_sandbox(io, &made, None, token);
        }
    }

    // Puts the directories under `/pumpkin` that every sandbox needs before its first command.
    fn set_up(&mut self, io: &mut Io<'r, 's, H>, token: u64) {
        let Some(start) = self.starts.get_mut(&token) else {
            unreachable!("a start's set-up is sent for no start")
        };
        let Some(session) = start.session else {
            unreachable!("a start sets its sandbox up only once its session is open")
        };
        start.step = Step::SettingUp;
        let by = start.by;
        Sessions::set_up(self, io, session, (by, token));
    }

    fn start_done(&mut self, io: &mut Io<'r, 's, H>, token: u64) {
        let Some(start) = self.starts.remove(&token) else {
            unreachable!("a start ended twice")
        };
        let (Some(id), Some(session)) = (start.id, start.session) else {
            unreachable!("a start ends only once its sandbox is created and its session open")
        };
        // The work's start waits on a shared role's set-up, which the loop sees end here.
        if let Which::Shared(target) = start.which {
            self.shared_ready(target, self.at, Ok(()));
        }
        match start.waiter {
            StartWaiter::Agent(agent) => self.agent_ready(io, agent, (id, session)),
            StartWaiter::Thread(arrivals, reply) => {
                let remote = self.sessions.remote(session, arrivals);
                let started = Started {
                    which: start.which,
                    id,
                    session: remote,
                };
                // A thread that has ended has nothing left to start.
                let _ = reply.send(Ok(started));
            }
        }
    }

    fn start_failed(&mut self, io: &mut Io<'r, 's, H>, token: u64, unstarted: Unstarted) {
        let Some(start) = self.starts.remove(&token) else {
            unreachable!("a start failed twice")
        };
        if let Some(session) = start.session {
            Sessions::drop_session(self, io, session);
        }
        // What the sandbox was to meet the gateway on goes: the socket that the loop holds, or
        // the wait of a thread, which then ends.
        match start.meeting {
            Some(Meeting::Socket(socket)) => socket.withdraw(io),
            Some(Meeting::Elsewhere(_)) | None => {}
        }
        if let Some(given_up) = &start.given_up {
            given_up.store(true, Ordering::Relaxed);
        }
        if let Which::Shared(target) = start.which {
            self.shared_ready(target, self.at, Err(unstarted.reason().to_owned()));
        }
        match start.waiter {
            StartWaiter::Agent(agent) => self.agent_unstarted(io, agent, unstarted),
            StartWaiter::Thread(_, reply) => {
                let _ = reply.send(Err(unstarted));
            }
        }
    }

    /// Deletes `which`'s sandbox, `id`, on its provider, then drops its session, if `waiter` holds
    /// one, and records the delete.
    pub(crate) fn begin_delete(
        &mut self,
        io: &mut Io<'r, 's, H>,
        which: Which,
        id: SandboxId,
        waiter: DeleteWaiter,
    ) {
        let token = self.new_token();
        let deleting = Deleting {
            which,
            id: id.clone(),
            waiter,
            recording: 0,
            failure: None,
            deleted: None,
            logged: Vec::new(),
        };
        self.deletes.insert(token, deleting);
        let provider = self.provider(which);
        let run = self.sandboxes.run.clone();
        let edge = io.edge();
        let disposing = matches!(
            self.deletes.get(&token).map(|delete| &delete.waiter),
            Some(DeleteWaiter::Disposal(_))
        );
        let undeleted = id.clone();
        let spawned = self.spawn((&format!("delete-{}", which.name()), None), move || {
            let parts = disposing.then(|| match provider.console(&run, &id) {
                Ok(console) if console.text.trim().is_empty() => Vec::new(),
                Ok(console) => vec![format!("its console ends: {}", console.text.trim())],
                Err(error) => vec![format!("its console can't be read: {error}")],
            });
            let deleted = provider.delete(&run, std::slice::from_ref(&id));
            edge.send(Input::Edge(Edge::Deleted(token, parts, deleted)));
        });
        // A delete that didn't start failed, which the loop takes in its next batch, as it takes
        // a delete's end.
        if let Err(error) = spawned {
            let deleted = Undeleted::of(vec![(undeleted, error)]);
            let parts = disposing.then(Vec::new);
            self.events
                .send(Input::Edge(Edge::Deleted(token, parts, deleted)));
        }
    }

    fn provider(&self, which: Which) -> &'r dyn provider::Provider {
        let on = &self.sandboxes.placed.placement(which.role()).provider;
        self.sandboxes.providers.get(on).provider
    }

    // Takes a delete's end, and `parts` of its message if it is a disposal's.
    fn deleted(
        &mut self,
        io: &mut Io<'r, 's, H>,
        token: u64,
        parts: Option<Vec<String>>,
        deleted: &Result<(), Undeleted>,
    ) {
        let Some(delete) = self.deletes.get_mut(&token) else {
            unreachable!("a delete completed for no delete")
        };
        let failed = deleted.as_ref().copied().map_err(ToString::to_string);
        let mut session = None;
        delete.deleted = Some(match &mut delete.waiter {
            // A disposal's message says why its delete failed.
            DeleteWaiter::Disposal(disposing) => {
                disposing.parts.extend(parts.into_iter().flatten());
                if let Err(error) = &failed {
                    disposing.parts.push(format!("deleting it failed: {error}"));
                }
                failed
            }
            DeleteWaiter::Agent(_, held) => {
                session = held.take();
                failed.context("deleting the sandbox failed")
            }
            DeleteWaiter::Thread(_) => failed.context("deleting the sandbox failed"),
        });
        let (which, id) = (delete.which, delete.id.clone());
        if let Some(session) = session {
            Sessions::drop_session(self, io, session);
        }
        let on = &self.sandboxes.placed.placement(which.role()).provider;
        self.record_delete(io, token, (on, id), deleted);
    }

    /// Writes how `deleted` went for each of `ids` into its record, if this run created it, whose
    /// writes' ends go to `token`, a delete's, which waits for each, or none's, and returns the log
    /// entries of the delete, one per id it deleted, named by its sandbox if this run created it.
    pub(crate) fn record_deletes(
        &mut self,
        io: &mut Io<'r, 's, H>,
        token: u64,
        on: &'r Name,
        ids: &[SandboxId],
        deleted: &Result<(), Undeleted>,
    ) -> Vec<Entry> {
        let undeleted = deleted.as_ref().err();
        let failed = |id: &SandboxId| undeleted.and_then(|undeleted| undeleted.why(id));
        let made: Vec<_> = (self.inventory.created.iter_mut())
            .filter(|made| made.on == on && ids.contains(&made.id))
            .map(|made| {
                made.deleted = failed(&made.id).is_none();
                made.clone()
            })
            .collect();
        let mut logged = Vec::new();
        for id in ids.iter().filter(|id| failed(id).is_none()) {
            let made = made.iter().find(|made| made.id == *id);
            let deleted = crate::sandbox::Deleted {
                sandbox: made.map(|made| made.which.name()),
                provider: on.clone(),
                id: id.clone(),
            };
            logged.push(Entry::Deleted(deleted));
        }
        // Counted before any is asked for, as one that fails as it is asked for ends at once.
        if let Some(delete) = self.deletes.get_mut(&token) {
            delete.recording += made.len();
        }
        for made in &made {
            let deleted = failed(&made.id).map_or(Ok(()), Err);
            self.record_sandbox(io, made, Some(deleted), token);
        }
        logged
    }

    // Records a delete of the machine's, whose writes' ends go to `token`, and logs it once they
    // are done.
    fn record_delete(
        &mut self,
        io: &mut Io<'r, 's, H>,
        token: u64,
        (on, id): (&'r Name, SandboxId),
        deleted: &Result<(), Undeleted>,
    ) {
        let Some(delete) = self.deletes.get_mut(&token) else {
            unreachable!("a delete records itself while it is under way")
        };
        // One more than its writes, which ends once its log entries are kept.
        delete.recording = 1;
        let logged = self.record_deletes(io, token, on, &[id], deleted);
        let Some(mut delete) = self.deletes.remove(&token) else {
            unreachable!("a delete waits for one more write than it asks for")
        };
        delete.logged = logged;
        self.delete_written(io, (token, delete), Ok(()));
    }

    /// Deletes what each provider that `include` names still lists of the run, and what the run
    /// created there and hasn't deleted, and records each delete. Waits on the loop for each
    /// record.
    pub(crate) fn sweep_on(
        &mut self,
        io: &mut Io<'r, 's, H>,
        include: impl Fn(&Name) -> bool,
    ) -> Result<(), String> {
        let sandboxes = self.sandboxes;
        let remaining: Vec<_> = (self.inventory.created.iter())
            .filter(|made| !made.deleted)
            .map(|made| (made.on.clone(), made.id.clone()))
            .collect();
        let mut failures = Vec::new();
        for (name, hosting) in sandboxes.providers.iter().filter(|(name, _)| include(name)) {
            // A sandbox that its provider lists no more may have ended and left what only its
            // delete removes.
            let held: Vec<SandboxId> = (remaining.iter())
                .filter(|(on, _)| on == name)
                .map(|(_, id)| id.clone())
                .collect();
            let listed = hosting.provider.list(&sandboxes.run).context(format_args!(
                "listing the run's sandboxes on provider `{name}` failed"
            ));
            let listed = match listed {
                Ok(listed) => listed,
                Err(error) => {
                    failures.push(error);
                    continue;
                }
            };
            let ids: Vec<SandboxId> = listed.into_iter().map(|listed| listed.id).collect();
            let unlisted = held.iter().filter(|id| !ids.contains(id)).cloned();
            let deleting: Vec<SandboxId> = ids.iter().cloned().chain(unlisted).collect();
            if deleting.is_empty() {
                continue;
            }
            let deleted = hosting.provider.delete(&sandboxes.run, &deleting);
            let token = self.new_token();
            let logged = self.record_deletes(io, token, name, &deleting, &deleted);
            while self.disk.busy() {
                self.run(io, Duration::MAX);
            }
            for entry in logged {
                self.logged(entry);
            }
            if let Err(error) = deleted {
                failures.push(format!(
                    "deleting the sandboxes the run left on provider `{name}` failed: {error}"
                ));
            }
            failures.extend(self.failure.take());
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }

    /// Takes the end of a write of `delete`'s record, taken out of the deletes under way, where
    /// it goes back while it waits for another.
    pub(crate) fn delete_written(
        &mut self,
        io: &mut Io<'r, 's, H>,
        (token, mut delete): (u64, Deleting),
        written: Result<(), String>,
    ) {
        if let Err(error) = written {
            delete.failure = Some(match delete.failure.take() {
                Some(before) => format!("{before}; {error}"),
                None => error,
            });
        }
        delete.recording = delete.recording.saturating_sub(1);
        if delete.recording > 0 {
            self.deletes.insert(token, delete);
            return;
        }
        for entry in delete.logged {
            self.logged(entry);
        }
        let recorded = delete.failure.map_or(Ok(()), Err);
        let deleted = delete.deleted.unwrap_or(Ok(()));
        match delete.waiter {
            DeleteWaiter::Agent(agent, _) => {
                self.agent_deleted(agent, crate::failed_also(deleted, recorded));
            }
            DeleteWaiter::Thread(reply) => {
                let _ = reply.send((crate::failed_also(deleted, recorded), io.edge()));
            }
            DeleteWaiter::Disposal(disposing) => {
                let Disposing { mut parts, waiter } = *disposing;
                let disposed = deleted.is_ok() && recorded.is_ok();
                parts.extend(recorded.err());
                let reason = parts.join("; ");
                let result = if disposed { Ok(reason) } else { Err(reason) };
                match waiter {
                    DisposeWaiter::Agent(agent) => self.agent_disposed(agent, result),
                    DisposeWaiter::Thread(reply) => {
                        let _ = reply.send(result);
                    }
                }
            }
        }
    }

    /// Deletes a sandbox that didn't start: the message is `reason` with the sandbox's console and
    /// the failure of the delete or of its record, if any.
    pub(crate) fn dispose(
        &mut self,
        io: &mut Io<'r, 's, H>,
        (which, id): (Which, SandboxId),
        reason: &str,
        waiter: DisposeWaiter,
    ) {
        let parts = vec![format!("sandbox {}: {reason}", id.as_str())];
        let disposing = Box::new(Disposing { parts, waiter });
        self.begin_delete(io, which, id, DeleteWaiter::Disposal(disposing));
    }
}

// The meeting over the network of a sandbox that a provider's create made.
fn elsewhere(registration: Registration) -> Elsewhere {
    let Registration::Elsewhere(elsewhere) = registration else {
        unreachable!("a meeting over the network is no socket's")
    };
    elsewhere
}

impl<'r, 's, H: Host + 'r> Runtime<'r, 's, H> {
    /// After work drains, sweeps providers whose failed creates could have allocated an instance.
    /// A provider can list such an instance only later. It never received a work command.
    pub(crate) fn finish_work(&mut self, io: &mut Io<'r, 's, H>) -> Result<(), String> {
        let unnamed = std::mem::take(&mut self.inventory.unnamed);
        if unnamed.is_empty() {
            return Ok(());
        }
        self.sweep_on(io, |name| unnamed.contains(name))
    }
}
