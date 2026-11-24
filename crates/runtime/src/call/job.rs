//! A call of a body, a job function's or the final call's: the loop starts the call's target, or
//! takes its turn on its shared role's, and its body runs on a worker of its own
//! ([`body::evaluate`](crate::body::evaluate)), which asks the loop for each of its target's
//! requests, its command's exit, its runner op's receipt and whether the call goes on, and waits
//! for each answer. The loop then reports what the body returned, and deletes a target of the
//! call's own.
//!
//! The worker holds an edge of the loop's while it runs ([`Io::edge`]), which it hands back with
//! each ask, and takes again with the answer: a simulated world never moves its clock while a
//! body works.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::Duration;

use decision::{CallResult, Event, JobId, Request};
use gateway::{Answer, Arrival, Ask, Process, SessionId, Sessions};
use schema::ir::{AbsolutePath, Name, TargetLifetime};
use schema::sandbox::op::{LeaseStatusReply, SessionOp};
use schema::sandbox::tree::Limits;
use schema::sandbox::{Entry, ExitStatus, Launch, RelativePath, pumpkin_dir};
use schema::workflow::{AgentId, TargetId, Workflow};
use stdx::io::{Edge, Events};

use crate::body::{self, Bodies, Ended, Of, Stdin};
use crate::child::{Child, End, uncollected};
use crate::record::Stored;
use crate::sandbox::PUMPKIN;
use crate::scoped::Waiting;
use crate::{Entry as Logged, Host, Input, Io, Purpose, Runtime, Targeted};

/// A call of a body, as its worker drives it.
pub(crate) struct Call<'r> {
    pub(crate) job: JobId,
    pub(crate) of: Of<'r>,
    pub(crate) inputs: Vec<Stored>,
    /// The longest the body may drive the target.
    pub(crate) timeout: Duration,
}

/// A call, from its start until its end is reported, and what the run knows of the role that
/// runs it.
pub(crate) struct Called<'r> {
    pub(crate) child: Child,
    // The role that runs the call, whose model route serves once the call's body starts.
    pub(crate) target: TargetId,
    pub(crate) instance: u32,
    pub(crate) owner: Option<AgentId>,
    // How many of the call's model requests its route has barred for a spent model budget.
    pub(crate) model_requests_over_budget: u64,
    /// The target's session, once it is ready: its own sandbox's, or its shared role's.
    pub(crate) session: Option<SessionId>,
    body: Body<'r>,
    stop: Stop,
    // Why the target's session ended, or its shared role failed, which every ask of the body's
    // then fails with.
    ended: Option<String>,
    // The exits of the target's commands that the body hasn't waited for yet.
    exits: Vec<(Process, ExitStatus)>,
    // What the target handed over for the core while the body worked, which the core takes once
    // the body asks next, after what the body logged meanwhile.
    held: Vec<Arrival>,
}

enum Body<'r> {
    // Its call, until its target is ready.
    Waiting(Call<'r>),
    // On its worker, which waits for the answer to its ask on the channel, if it asked, for what
    // the wait says if it waits on the loop.
    Working {
        answers: SyncSender<(Told, Edge<Input>)>,
        asking: bool,
        waits: Option<Wait>,
    },
    Ended,
}

/// A worker's wait on the loop, until its deadline.
struct Wait {
    on: Waits,
    until: Duration,
}

enum Waits {
    Exit(Process),
    Receipt,
}

// The core's stop, and whether the body has heard it: it hears it once, as a stop cuts the call
// short once.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    No,
    Unheard,
    Heard,
}

/// What a body's worker asks the loop, each answered by a [`Told`].
pub(crate) enum Asked {
    /// Whether the core has stopped the call, which the body hears once, unless the target has
    /// ended.
    Going,
    /// A request of the target's session, by the deadline.
    Session(Ask, Duration),
    /// The exit of the target's command, by the deadline, unless the call is stopped first.
    Exit(Process, Duration),
    /// An op of a runner's, which the core answers with a lease, whose receipt it waits for by
    /// the deadline, unless the call is stopped first.
    Runner(Name, SessionOp, Waiting, Duration),
    /// Why the shared role's session has ended, if it has.
    Ended,
    /// The body's end, with the shared role's standard input as the body left it; the worker's
    /// last ask, which nothing answers.
    Done(End, Stdin),
}

/// How a body's wait on the loop ended.
pub(crate) enum Waited<T> {
    Finished(T),
    /// Its deadline passed first.
    TimedOut,
    /// The core stopped the call first.
    Stopped,
}

impl<T> Waited<T> {
    fn map<U>(self, finished: impl FnOnce(T) -> U) -> Waited<U> {
        match self {
            Self::Finished(done) => Waited::Finished(finished(done)),
            Self::TimedOut => Waited::TimedOut,
            Self::Stopped => Waited::Stopped,
        }
    }
}

/// The loop's answer to an [`Asked`], at the time of the loop's batch, since the run started.
pub(crate) struct Told {
    now: Duration,
    told: Telling,
}

enum Telling {
    Going(Result<bool, String>),
    Answer(Answer),
    // How a wait ended, or why the target ended first.
    Waited(Result<Waited<Finish>, String>),
    Ended(Option<String>),
}

// What a wait on the loop finished with.
enum Finish {
    Exit(ExitStatus),
    Receipt(LeaseStatusReply),
}

/// The loop, as a body's worker reaches it: each ask goes through the edge that it holds, and the
/// answer comes with the next.
pub(crate) struct OnLoop {
    job: JobId,
    edge: Option<Edge<Input>>,
    told: Receiver<(Told, Edge<Input>)>,
    events: Events<Input>,
    now: Duration,
}

impl OnLoop {
    /// The time of the loop's last answer, since the run started.
    pub(crate) fn now(&self) -> Duration {
        self.now
    }

    fn ask(&mut self, asked: Asked) -> Telling {
        let Some(edge) = self.edge.take() else {
            unreachable!("a body's worker asks once its last answer has come, with its edge")
        };
        edge.send(Input::Body(self.job, asked));
        let Ok((Told { now, told }, edge)) = self.told.recv() else {
            unreachable!(
                "the loop dropped a body's ask unanswered, though it runs until every thread of \
                 the run has ended, answering each ask"
            )
        };
        self.now = now;
        self.edge = Some(edge);
        told
    }

    pub(crate) fn going(&mut self) -> Result<bool, String> {
        match self.ask(Asked::Going) {
            Telling::Going(going) => going,
            _ => unreachable!("the loop answered whether a call goes on with another answer"),
        }
    }

    pub(crate) fn exit(
        &mut self,
        process: Process,
        until: Duration,
    ) -> Result<Waited<ExitStatus>, String> {
        let waited = self.wait(Asked::Exit(process, until))?;
        Ok(waited.map(|finished| match finished {
            Finish::Exit(status) => status,
            Finish::Receipt(_) => {
                unreachable!("the loop answered a wait for an exit with a receipt")
            }
        }))
    }

    pub(crate) fn runner(
        &mut self,
        (grant, op): (&Name, SessionOp),
        waiting: Waiting,
        until: Duration,
    ) -> Result<Waited<LeaseStatusReply>, String> {
        let waited = self.wait(Asked::Runner(grant.clone(), op, waiting, until))?;
        Ok(waited.map(|finished| match finished {
            Finish::Receipt(status) => status,
            Finish::Exit(_) => unreachable!("the loop answered a runner's op with an exit"),
        }))
    }

    fn wait(&mut self, asked: Asked) -> Result<Waited<Finish>, String> {
        match self.ask(asked) {
            Telling::Waited(waited) => waited,
            _ => unreachable!("the loop answered a wait with another answer"),
        }
    }

    pub(crate) fn ended(&mut self) -> Option<String> {
        match self.ask(Asked::Ended) {
            Telling::Ended(ended) => ended,
            _ => unreachable!("the loop answered a session's end with another answer"),
        }
    }

    fn session(&mut self, ask: Ask, by: Duration) -> Answer {
        match self.ask(Asked::Session(ask, by)) {
            Telling::Answer(answer) => answer,
            _ => unreachable!("the loop answered a session's request with another answer"),
        }
    }

    /// Asks the target's session a request that is done or fails: a put, a freeze or a kill.
    pub(crate) fn done(&mut self, ask: Ask, by: Duration) -> std::io::Result<()> {
        match self.session(ask, by) {
            Answer::Done(done) => done,
            other => unreachable!("{other:?} answered a request that is done or fails"),
        }
    }

    pub(crate) fn start(&mut self, launch: Launch, by: Duration) -> std::io::Result<Process> {
        match self.session(Ask::Start(launch), by) {
            Answer::Started(started) => started,
            other => unreachable!("{other:?} answered a start"),
        }
    }

    pub(crate) fn collect(
        &mut self,
        (path, into): (&AbsolutePath, &Path),
        limits: Limits,
        by: Duration,
    ) -> std::io::Result<Vec<Entry>> {
        match self.session(Ask::Collect(path.clone(), into.to_owned(), limits), by) {
            Answer::Collected(collected) => collected,
            other => unreachable!("{other:?} answered a collect"),
        }
    }

    /// Collects `dir` of the sandbox's `.pumpkin` directory into `into`, a new directory, within
    /// the `PUMPKIN` limits, by `by`. After a failure it holds what arrived before.
    pub(crate) fn collect_dir(
        &mut self,
        dir: &RelativePath,
        into: &Path,
        by: Duration,
    ) -> Result<(), String> {
        let from = pumpkin_dir().join(dir);
        let collected = self.collect((&from, into), PUMPKIN, by);
        collected
            .map(drop)
            .map_err(|error| uncollected(dir, &error))
    }

    pub(crate) fn event(&self, event: Event) {
        self.events.send(Input::Core(event));
    }

    /// Logs what the body asked of the target, at the time of the loop's last answer.
    pub(crate) fn log(&self, targeted: Targeted) {
        let at = self.now;
        self.events
            .send(Input::Logged(Logged::Target(self.job, targeted, at)));
    }

    pub(crate) fn note(&self, note: String) {
        self.events.send(Input::Logged(Logged::Note(note)));
    }

    fn end(mut self, end: End, stdin: Stdin) {
        let Some(edge) = self.edge.take() else {
            unreachable!("a body's worker ends once its last answer has come, with its edge")
        };
        edge.send(Input::Body(self.job, Asked::Done(end, stdin)));
    }
}

/// The body's end of `call`, called through `bodies` on the target that `on` reaches.
fn evaluate(
    call: Call<'_>,
    bodies: &dyn Bodies,
    (workflow, dir, collect): (&Workflow, &Path, Duration),
    on: &mut OnLoop,
    stdin: &mut Stdin,
) -> End {
    let Call {
        job,
        of,
        inputs,
        timeout,
    } = call;
    let call = body::Call {
        job,
        target: workflow.target(of.target()),
        workflow,
        of: &of,
        inputs,
        timeout,
    };
    let end = match body::evaluate(bodies, &call, on, (dir, collect), stdin) {
        Ended::Returned(returned) => CallResult::returned_by(returned),
        Ended::Failed(reason) => Err(reason),
        Ended::TimedOut => Err("the call's timeout passed".to_owned()),
        Ended::Stopped => Err("the call was stopped".to_owned()),
    };
    End::Call(job, end)
}

impl<'r> Called<'r> {
    pub(crate) fn new(
        child: Child,
        call: Call<'r>,
        (instance, owner): (u32, Option<AgentId>),
    ) -> Self {
        Self {
            child,
            target: call.of.target(),
            instance,
            owner,
            model_requests_over_budget: 0,
            session: None,
            body: Body::Waiting(call),
            stop: Stop::No,
            ended: None,
            exits: Vec::new(),
            held: Vec::new(),
        }
    }

    /// Whether its body runs.
    pub(crate) fn busy(&self) -> bool {
        matches!(self.body, Body::Working { .. })
    }

    // The worker's wait, if it waits on the loop.
    fn waits(&mut self) -> Option<&mut Option<Wait>> {
        match &mut self.body {
            Body::Working { waits, .. } => Some(waits),
            Body::Waiting(_) | Body::Ended => None,
        }
    }
}

impl<'r, 's, H: Host + 'r> Runtime<'r, 's, H> {
    /// Runs call `job`'s body on its target, whose session is `session`, on a worker of its own.
    pub(crate) fn work(&mut self, io: &mut Io<'r, 's, H>, job: JobId, session: SessionId) {
        let called = self.call(job);
        let Body::Waiting(call) = std::mem::replace(&mut called.body, Body::Ended) else {
            unreachable!("a call's body runs once")
        };
        called.session = Some(session);
        let resident = self.workflow().target(call.of.target()).lifetime == TargetLifetime::Work;
        let mut stdin = if resident {
            (self.residents.get_mut(&call.of.target()))
                .and_then(|resident| resident.stdin.take())
                .unwrap_or_default()
        } else {
            Stdin::default()
        };
        let (answers, told) = mpsc::sync_channel(1);
        let on = OnLoop {
            job,
            edge: Some(io.edge()),
            told,
            events: self.events.clone(),
            now: self.at,
        };
        let (bodies, workflow) = (self.bodies, self.workflow());
        let called = self.call(job);
        called.body = Body::Working {
            answers,
            asking: false,
            waits: None,
        };
        let (dir, collect): (PathBuf, Duration) =
            (called.child.dir.clone(), called.child.deadlines.collect);
        let name = called.child.which.name().to_string();
        let spawned = self.spawn((&name, Some(bodies.stack_bytes())), move || {
            let mut on = on;
            let end = evaluate(call, bodies, (workflow, &dir, collect), &mut on, &mut stdin);
            on.end(end, stdin);
        });
        if let Err(reason) = spawned {
            self.body_done(io, job, End::Call(job, Err(reason)), None);
        }
    }

    /// Takes what call `job`'s worker asks.
    pub(crate) fn body_asked(&mut self, io: &mut Io<'r, 's, H>, job: JobId, asked: Asked) {
        let at = self.at;
        let Some(called) = self.jobs.get_mut(&job) else {
            unreachable!("a body asked for no call")
        };
        if let Body::Working { asking, .. } = &mut called.body {
            *asking = true;
        }
        let held = std::mem::take(&mut called.held).into_iter();
        (self.inputs).extend(held.map(|arrival| (at, Input::Call(job, arrival))));
        let told = match asked {
            Asked::Going => match (&called.ended, called.stop) {
                (Some(reason), _) => Telling::Going(Err(reason.clone())),
                (None, stop) => {
                    let stopped = stop == Stop::Unheard;
                    if stopped {
                        called.stop = Stop::Heard;
                    }
                    Telling::Going(Ok(stopped))
                }
            },
            Asked::Session(ask, by) => {
                let Some(session) = called.session else {
                    unreachable!("a body asks of its target once its session is open")
                };
                let token = self.token(Purpose::Body(job));
                Sessions::ask(self, io, session, ask, (by, token));
                return;
            }
            Asked::Exit(process, until) => {
                if let Some(at) = (called.exits.iter()).position(|(exited, _)| *exited == process) {
                    let (_, status) = called.exits.remove(at);
                    Telling::Waited(Ok(Waited::Finished(Finish::Exit(status))))
                } else {
                    match called.resolve(at, Waits::Exit(process), until) {
                        Some(waited) => Telling::Waited(waited),
                        None => return,
                    }
                }
            }
            Asked::Runner(grant, op, waiting, until) => {
                let request = Request::Session { grant, op };
                (self.inputs).push_back((at, Input::Runner(job, request, waiting)));
                match called.resolve(at, Waits::Receipt, until) {
                    Some(waited) => Telling::Waited(waited),
                    None => return,
                }
            }
            Asked::Ended => Telling::Ended(
                (self.residents.get(&called.target))
                    .and_then(|resident| resident.session_end.clone()),
            ),
            Asked::Done(end, stdin) => return self.body_done(io, job, end, Some(stdin)),
        };
        self.tell(io, job, told);
    }

    // Answers call `job`'s worker.
    fn tell(&mut self, io: &mut Io<'r, 's, H>, job: JobId, told: Telling) {
        let Some(Body::Working {
            answers,
            asking,
            waits,
        }) = self.jobs.get_mut(&job).map(|c| &mut c.body)
        else {
            unreachable!("the loop answered a body that doesn't work")
        };
        *waits = None;
        *asking = false;
        let told = Told { now: self.at, told };
        // A worker always waits for the answer to its ask, which only ends its thread by a panic,
        // which ends the process.
        let _ = answers.send((told, io.edge()));
    }

    /// Takes the answer to a request of call `job`'s body's.
    pub(crate) fn body_answered(&mut self, io: &mut Io<'r, 's, H>, job: JobId, answer: Answer) {
        self.tell(io, job, Telling::Answer(answer));
    }

    /// Takes the receipt of call `job`'s body's runner op, which the body waits for unless its
    /// wait has ended.
    pub(crate) fn answer_body(
        &mut self,
        io: &mut Io<'r, 's, H>,
        job: JobId,
        result: Result<LeaseStatusReply, String>,
    ) {
        let waits = self.jobs.get_mut(&job).and_then(Called::waits);
        if let Some(waits) = waits
            && matches!(
                waits,
                Some(Wait {
                    on: Waits::Receipt,
                    ..
                })
            )
        {
            let told =
                Telling::Waited(result.map(|status| Waited::Finished(Finish::Receipt(status))));
            self.tell(io, job, told);
        }
    }

    /// Takes what call `job`'s target's session handed over: its command's exit, or its end.
    pub(crate) fn call_arrived(&mut self, io: &mut Io<'r, 's, H>, job: JobId, arrival: Arrival) {
        let called = self.call(job);
        match arrival {
            Arrival::Exited { process, status } => {
                let waits = called.waits();
                if let Some(waits) = waits
                    && matches!(waits, Some(Wait { on: Waits::Exit(of), .. }) if *of == process)
                {
                    let exited = Waited::Finished(Finish::Exit(status));
                    return self.tell(io, job, Telling::Waited(Ok(exited)));
                }
                called.exits.push((process, status));
            }
            Arrival::Ended { reason } => self.call_ends(io, job, reason),
            // The core takes these in order with the rest of what reaches the loop, once the body
            // has asked for what it does next: a body works at no time of the run's.
            arrival @ (Arrival::Op(_) | Arrival::Refused(_) | Arrival::ModelRequest(_)) => {
                match called.body {
                    Body::Working { asking: false, .. } => called.held.push(arrival),
                    _ => self.input(Input::Call(job, arrival)),
                }
            }
        }
    }

    /// Ends call `job`'s target for its body, for `reason`: every ask of the body's fails with it
    /// from now on.
    pub(crate) fn call_ends(&mut self, io: &mut Io<'r, 's, H>, job: JobId, reason: String) {
        let called = self.call(job);
        called.ended.get_or_insert(reason.clone());
        if called.waits().is_some_and(|waits| waits.is_some()) {
            self.tell(io, job, Telling::Waited(Err(reason)));
        }
    }

    /// Takes the core's stop of call `job`, which its body hears at its next ask, or now if it
    /// waits on the loop.
    pub(crate) fn stop_call(&mut self, io: &mut Io<'r, 's, H>, job: JobId) {
        let called = self.call(job);
        // The core stops a call once, and the run's end stops every one.
        if called.stop != Stop::No {
            return;
        }
        called.stop = Stop::Unheard;
        if called.waits().is_some_and(|waits| waits.is_some()) {
            called.stop = Stop::Heard;
            self.tell(io, job, Telling::Waited(Ok(Waited::Stopped)));
        }
    }

    /// The earliest deadline that a body waits on the loop until, as a time since the run started.
    pub(crate) fn bodies_due(&self) -> Option<Duration> {
        (self.jobs.values())
            .filter_map(|called| match &called.body {
                Body::Working {
                    waits: Some(wait), ..
                } => Some(wait.until),
                Body::Working { waits: None, .. } | Body::Waiting(_) | Body::Ended => None,
            })
            .min()
    }

    /// Ends each body's wait on the loop whose deadline has passed.
    pub(crate) fn time_out_bodies(&mut self, io: &mut Io<'r, 's, H>) {
        let at = self.at;
        let due: Vec<JobId> = (self.jobs.iter())
            .filter(|(_, called)| match &called.body {
                Body::Working {
                    waits: Some(wait), ..
                } => at >= wait.until,
                _ => false,
            })
            .map(|(job, _)| *job)
            .collect();
        for job in due {
            self.tell(io, job, Telling::Waited(Ok(Waited::TimedOut)));
        }
    }

    // Ends call `job`'s body: reports its end, around its target's delete if the call has one of
    // its own, and hands a shared role its standard input back.
    fn body_done(&mut self, io: &mut Io<'r, 's, H>, job: JobId, end: End, stdin: Option<Stdin>) {
        let target = self.end_unworked(io, job, end);
        if self.workflow().target(target).lifetime == TargetLifetime::Work {
            self.resident_done(io, target, stdin);
        }
    }

    /// Ends call `job`, whose body ran or never did: reports `end`, around the delete of its
    /// target if the call has one of its own. Returns its target.
    pub(crate) fn end_unworked(
        &mut self,
        io: &mut Io<'r, 's, H>,
        job: JobId,
        end: End,
    ) -> TargetId {
        let called = self.call(job);
        called.body = Body::Ended;
        let held = std::mem::take(&mut called.held);
        let target = called.target;
        let child = (called.child.which, called.child.started.take());
        for arrival in held {
            self.input(Input::Call(job, arrival));
        }
        self.close_child(io, child, end);
        target
    }
}

impl Called<'_> {
    // What a wait on `on` until `until` comes to now, at `at`, if it ends at once: parks it
    // otherwise.
    fn resolve(
        &mut self,
        at: Duration,
        on: Waits,
        until: Duration,
    ) -> Option<Result<Waited<Finish>, String>> {
        if let Some(reason) = &self.ended {
            return Some(Err(reason.clone()));
        }
        if self.stop == Stop::Unheard {
            self.stop = Stop::Heard;
            return Some(Ok(Waited::Stopped));
        }
        if at >= until {
            return Some(Ok(Waited::TimedOut));
        }
        if let Some(waits) = self.waits() {
            *waits = Some(Wait { on, until });
        }
        None
    }
}
