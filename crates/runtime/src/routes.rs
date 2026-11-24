//! The run's model routes, as its loop serves them: a route for each caller whose role has one,
//! from the caller's start until the core stops allowing its requests. Each request's exchange
//! upstream runs on a thread of its own, which hands its response to the loop a frame at a time,
//! and the loop sends each on the caller's session.

use std::io;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::time::Duration;

use decision::{Caller, Core, Outcome, WorkEnd};
use gateway::{Answer as Sent, Ask, IncomingModelRequest, Response, SessionId, Sessions};
use route::{Admission, Answer, Bar, Respond};
use schema::ir::{Api, Name};
use schema::sandbox::ToSandbox;
use schema::workflow::{AgentId, RoleId};
use stdx::io::Events;
use stdx::time::Clock as _;

use crate::budget::Budget;
use crate::log::{Entry, ModelAnswer};
use crate::model::{Exchange, Served};
use crate::{Host, Input, Io, Purpose, Runtime, Stopped};

// How many of a response's frames wait for the loop at most, besides the one it sends: as a put's
// frames do, so that a response streams at the sandbox's pace and holds two frames of it.
const RESPONSE_FRAMES: usize = 2;

/// A model response that a request's thread hands the loop, to send on the caller's session.
pub(crate) struct Responding {
    session: SessionId,
    request: u32,
    by: Duration,
    frames: Receiver<Response>,
    // Whether one of its frames is being sent.
    sending: bool,
}

// A response, as a request's thread hands it to the loop.
struct ToLoop {
    number: u64,
    // The session's number of the request.
    request: u32,
    frames: SyncSender<Response>,
    events: Events<Input>,
    ended: bool,
}

impl ToLoop {
    fn send(&self, frame: Response) -> io::Result<()> {
        if self.frames.send(frame).is_err() {
            let stopped = "the session stopped taking the response";
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, stopped));
        }
        self.events.send(Input::Respond(self.number));
        Ok(())
    }
}

impl Respond for ToLoop {
    fn head(&mut self, status: u16, headers: Vec<(String, String)>) -> io::Result<()> {
        // A head too long for a frame fails at once, which cuts the response short.
        let head = ToSandbox::ModelResponse {
            request: self.request,
            status,
            headers: headers.clone(),
        };
        schema::frame::write(&mut io::sink(), &head, &[])?;
        self.send(Response::Head { status, headers })
    }

    fn data(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.send(Response::Data(bytes.to_vec()))
    }

    fn end(&mut self, cut: bool) -> io::Result<()> {
        self.ended = true;
        self.send(Response::End { cut })
    }
}

impl Drop for ToLoop {
    fn drop(&mut self) {
        // A response that its route left unended is cut short, so that its client doesn't take it
        // as whole.
        if !self.ended {
            let _ = self.end(true);
        }
    }
}

impl<'r, 's, H: Host + 'r> Runtime<'r, 's, H> {
    pub(crate) fn model_request(
        &mut self,
        live: bool,
        core: &Core<'_>,
        (caller, at): (Caller, Duration),
        incoming: IncomingModelRequest,
    ) -> Result<(), String> {
        let number = self.next_model_request;
        self.next_model_request += 1;
        // A call's start and request may both arrive after the run ends, with no route installed.
        if !live {
            drop(incoming);
            let note = format!("model request {number} wasn't answered: the run has ended");
            return self.log.note(&note);
        }
        // No type excludes a model request here, since one `Arrival` serves sessions with model
        // requests and without.
        let Some(served) = self.served.get(&caller).map(Arc::clone) else {
            unreachable!(
                "a model request arrived from {caller:?} with no route, though only a role with \
                 a route takes model requests, the agent's is served before its sandbox starts, \
                 and a call's body reports its start before it starts a command"
            )
        };
        let role = self.role(caller);
        let allowed = core.model_requests_allowed(caller, at);
        let spent = self.budgets.get(&role).map(Budget::spent);
        let admission = move |entry| match (allowed, &spent) {
            (false, _) => Admission::Ended,
            (true, None) => Admission::Open,
            (true, Some(spent)) => spent.admission(entry),
        };
        // A response still open at its deadline ends the caller's session, which its collect
        // needs, so it is due by then; the core's stop of its requests ends it before that
        // (`end_routes`).
        let by = match caller {
            Caller::Agent(_) => self.teardown_end(),
            Caller::Job(job) => self.call(job).child.deadlines.collect,
        };
        let reports = self.events.clone();
        let mut client = self.respond_to(number, (incoming.session, incoming.number), by);
        let clock = self.clock.clone();
        let exchange = Exchange::new(self.dir, number, clock.clone());
        // A request stays in flight until its response's end is sent, which follows the thread's
        // last step, so the session's bound on the requests in flight,
        // `MAX_MODEL_REQUESTS_IN_FLIGHT`, bounds these threads too, but for those ending.
        let spawned = self.spawn((&format!("model-{number}"), None), move || {
            // The loop's inbox outlives every request's thread, which the run's end waits for.
            // Both reports come before the answer's end, and so before what the caller does
            // next. Each time is read here, as it happens, since the loop writes the record
            // later, by however long it is busy.
            let forwarding = || {
                let started = Entry::ModelRequestStarted((caller, number), clock.now());
                reports.send(Input::Logged(started));
            };
            let answered = |taken: Option<&(Name, Api)>, answer| {
                let answered = ModelAnswer {
                    taken: taken.cloned(),
                    answer,
                    kept: exchange.kept(),
                    first: exchange.timed(),
                    ended: clock.now(),
                };
                let answered = Entry::ModelAnswered((caller, number), Box::new(answered));
                reports.send(Input::Logged(answered));
            };
            served.serve(
                (number, &exchange),
                (&incoming.request, &mut client),
                admission,
                by,
                forwarding,
                answered,
            );
            if let Some(failed) = exchange.finish() {
                let note = format!("model request {number} wasn't kept whole: {failed}");
                reports.send(Input::Logged(Entry::Note(note)));
            }
        });
        match spawned {
            Ok(()) => Ok(()),
            Err(error) => {
                (self.log).note(&format!("model request {number} wasn't answered: {error}"))
            }
        }
    }

    // Where the thread of request `number`, the session's `request`, hands its response to the
    // loop, which sends it on the session by `by`: a session dropped meanwhile ends it.
    fn respond_to(
        &mut self,
        number: u64,
        (session, request): (SessionId, u32),
        by: Duration,
    ) -> ToLoop {
        let (frames, taken) = mpsc::sync_channel(RESPONSE_FRAMES);
        let responding = Responding {
            session,
            request,
            by,
            frames: taken,
            sending: false,
        };
        self.responding.insert(number, responding);
        ToLoop {
            number,
            request,
            frames,
            events: self.events.clone(),
            ended: false,
        }
    }

    /// Sends the next frame of the response to model request `number` that its thread handed the
    /// loop, unless one is being sent.
    pub(crate) fn respond_next(&mut self, io: &mut Io<'r, 's, H>, number: u64) {
        let Some(responding) = self.responding.get_mut(&number) else {
            return;
        };
        if responding.sending {
            return;
        }
        let frame = match responding.frames.try_recv() {
            Ok(frame) => frame,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.responding.remove(&number);
                return;
            }
        };
        responding.sending = true;
        let end = matches!(frame, Response::End { .. });
        let (session, request, by) = (responding.session, responding.request, responding.by);
        let token = self.token(Purpose::Respond(number, end));
        Sessions::ask(self, io, session, Ask::Respond(request, frame), (by, token));
    }

    /// Takes the send of a frame of the response to model request `number`, its end if `end`.
    pub(crate) fn responded(
        &mut self,
        io: &mut Io<'r, 's, H>,
        (number, end): (u64, bool),
        sent: Sent,
    ) {
        let Some(responding) = self.responding.get_mut(&number) else {
            unreachable!("a frame was sent of a response that the loop holds until it ends")
        };
        responding.sending = false;
        match sent {
            Sent::Done(Ok(())) if !end => self.respond_next(io, number),
            // A response ends with its end, or once the session stops taking it, which the thread
            // hears as it next hands over a frame. No frame of a response is too long for the
            // protocol: its head is checked as the thread hands it over, and its data is a
            // frame's at most.
            Sent::Done(_) => {
                self.responding.remove(&number);
            }
            other => unreachable!("{other:?} answered a model response's frame"),
        }
    }

    pub(crate) fn count(&mut self, entry: &Entry) {
        if let Entry::ModelAnswered((caller, _), answered) = entry {
            let role = self.role(*caller);
            if let (Some(budget), Some((_, api))) = (self.budgets.get_mut(&role), &answered.taken) {
                budget.add(*api, &answered.answer);
            }
            if let Caller::Job(job) = caller
                && let Answer::Barred {
                    bar: Bar::Budget, ..
                } = answered.answer
            {
                self.call(*job).model_requests_over_budget += 1;
            }
        }
    }

    fn role(&mut self, caller: Caller) -> RoleId {
        match caller {
            Caller::Agent(agent) => agent.role(),
            Caller::Job(job) => self.call(job).target.role(),
        }
    }

    pub(crate) fn serve(&mut self, caller: Caller) {
        let role = self.role(caller);
        if let Some(served) = Served::new(self.routes.get(role)) {
            self.served.insert(caller, Arc::new(served));
        }
    }

    pub(crate) fn end_routes(&self, allowing: Option<(&Core<'_>, Duration)>) {
        for (caller, served) in &self.served {
            if allowing.is_none_or(|(core, at)| !core.model_requests_allowed(*caller, at)) {
                served.end();
            }
        }
    }

    pub(crate) fn owner(&self, caller: Caller) -> Option<AgentId> {
        match caller {
            Caller::Agent(agent) => Some(agent),
            Caller::Job(job) => self.jobs.get(&job).and_then(|call| call.owner),
        }
    }

    // A work route's failure belongs to its agent. The final target's route belongs to the run.
    pub(crate) fn routes_held(&self, outcome: &mut Result<Outcome, Stopped>) -> Result<(), String> {
        for (&caller, served) in &self.served {
            let Some(name) = served
                .key_refused_by()
                .filter(|_| !self.held_at_lock.contains(&caller))
            else {
                continue;
            };
            let route = match caller {
                Caller::Agent(_) => "the agent's".to_owned(),
                Caller::Job(job) => format!("job {}'s", job.0),
            };
            let reason = format!(
                "the upstream `{name}` refused its key, with a 401, which ended {route} \
                                  model route: give it a key that it takes, and run again"
            );
            let owner = self.owner(caller);
            if let Ok(outcome) = outcome
                && let Some(agent) = outcome
                    .agents
                    .iter_mut()
                    .find(|agent| Some(agent.agent) == owner)
            {
                agent.route_failure.get_or_insert(reason);
            } else {
                return Err(reason);
            }
        }
        if let Ok(outcome) = outcome {
            for agent in &mut outcome.agents {
                let caller = Caller::Agent(agent.agent);
                if agent.route_failure.is_none()
                    && matches!(agent.work_end, WorkEnd::AgentExited(_))
                    && !self.held_at_lock.contains(&caller)
                    && let Some(route) = self.served.get(&caller)
                {
                    agent.route_failure = route.failed_last().err();
                }
            }
        }
        Ok(())
    }
}
