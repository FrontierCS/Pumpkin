//! The core's turns: each input the loop took becomes the core's event, in the order the inputs
//! reached the loop, and a tick comes once the core's next act is due.

use std::time::Duration;

use decision::{Action, Caller, Core, Event, Outcome};
use gateway::{Arrival, Sessions};

use crate::child::Report;
use crate::{Asked, Host, Input, Io, Runtime, Stopped, call, failed_also, interruption, request};

impl<'r, 's, H: Host + 'r> Runtime<'r, 's, H> {
    pub(crate) fn turns(
        &mut self,
        io: &mut Io<'r, 's, H>,
        core: &mut Core<'_>,
        mut actions: Vec<Action>,
    ) -> Result<Result<Outcome, String>, Stopped> {
        loop {
            if let Some(end) = self.perform(io, core, actions)? {
                return Ok(end);
            }
            let (at, event) = self.next_event(io, core)?;
            // The decision is recorded below, before it is carried out at the top of the next turn.
            let locking = match &event {
                Event::OpArrived {
                    caller: Caller::Agent(agent),
                    ..
                } if !core.submission_locked(*agent) => Some(*agent),
                _ => None,
            };
            actions = core.handle(at, event.clone());
            // A call's route is open from its command's or its body's start, as the agent's is
            // from the run's.
            if let Event::CallStarted { job } = event {
                self.serve(Caller::Job(job));
            }
            // The keys' refusals are read as the core locks the submission, before the record of it
            // is written, so that a refusal heard after the lock leaves the run to be judged.
            if let Some(agent) = locking.filter(|id| core.submission_locked(*id)) {
                let held = self
                    .served
                    .iter()
                    .filter_map(|(&caller, served)| {
                        (self.owner(caller) == Some(agent) && served.key_refused_by().is_none())
                            .then_some(caller)
                    })
                    .collect::<Vec<_>>();
                self.held_at_lock.extend(held);
            }
            let receipt = self.receipt(core, &event);
            let leads = receipt.is_some() || !actions.is_empty();
            self.log.event(self.workflow(), at, &event, &actions)?;
            // A line whose actions the runtime carries out next lasts before it does.
            if leads {
                self.synced(io)?;
            }
            if let Some((job, reply)) = receipt {
                self.answer_body(job, reply);
            }
            // A route ends as the core stops allowing its model requests, the agent's at the work
            // phase's end, a call's at its timeout or its report, before the core's actions are
            // performed: its exchanges in flight then end while the agent's teardown, if the
            // actions hold it, still needs the agent's session. The event that first stops the
            // agent's ends the work phase or the run, so its record lists the agent's teardown or
            // the run's abandon, and is synced before the route ends too.
            self.end_routes(Some((core, at)));
        }
    }

    fn next_event(
        &mut self,
        io: &mut Io<'r, 's, H>,
        core: &Core<'_>,
    ) -> Result<(Duration, Event), Stopped> {
        interruption::check()?;
        if let Some(event) = self.pending.pop_front() {
            return Ok(event);
        }
        let due = core.tick_due();
        loop {
            // The core takes nothing once a signal has come.
            interruption::check()?;
            if let Some(failure) = self.failure.take() {
                return Err(Stopped::Failed(failure));
            }
            if let Some((reached, input)) = self.inputs.pop_front() {
                // What reached the loop once the tick's time had come goes on after the tick, which
                // ends a phase or the run.
                if reached >= due {
                    self.inputs.push_front((reached, input));
                    // An input that waited behind a slow sync goes after the tick that came due
                    // before it reached the loop, at its due time; one that reached the loop in
                    // this batch goes after the tick of this batch's time.
                    let at = if reached < self.at { due } else { self.at };
                    return Ok((at, Event::Tick));
                }
                if let Some(event) = self.arrived(io, core, (reached, input), true)? {
                    return Ok(event);
                }
                continue;
            }
            if self.at >= due {
                return Ok((self.at, Event::Tick));
            }
            self.run(io, due);
        }
    }

    /// The core's event for `input`, which reached the loop at `at`, if it brings one. `live` is
    /// false once the run has ended, and a model request then goes unanswered.
    pub(crate) fn arrived(
        &mut self,
        io: &mut Io<'r, 's, H>,
        core: &Core<'_>,
        (at, input): (Duration, Input),
        live: bool,
    ) -> Result<Option<(Duration, Event)>, String> {
        let (id, arrival) = match input {
            Input::Agent(id, arrival) => {
                // An op or exit can beat the start's confirmation.
                if !matches!(arrival, Arrival::Ended { .. }) {
                    self.launched(id, at)?;
                }
                (id, arrival)
            }
            Input::Shared(target, arrival) => {
                let event = self
                    .residents
                    .get_mut(&target)
                    .and_then(|resident| resident.arrival(target, arrival));
                return Ok(event.map(|event| (at, event)));
            }
            Input::Core(event) => return Ok(Some((at, event))),
            Input::Unstarted(id, detail, torn) => {
                let role = &self.workflow().sandbox(id.role()).name;
                let note = format!("agent `{role}`: the agent's sandbox didn't start: {detail}");
                self.log.note(&note)?;
                if torn {
                    self.tear_down_agent(io, id);
                }
                return Ok(None);
            }
            Input::Child(report) => return self.report(core, report, live),
            Input::Logged(entry) => {
                self.count(&entry);
                return self.log.entry(entry).map(|()| None);
            }
            Input::Session(_)
            | Input::Start(_)
            | Input::Delete(_)
            | Input::Dispose(_)
            | Input::Edge(_)
            | Input::Exited => unreachable!(
                "the loop queued an input for the core that it serves itself, as it takes it"
            ),
        };
        // Ops are taken as usual even once the agent's session has closed: the core has ended the
        // work phase, or the run, by then, so it refuses each, and the run log notes that the
        // answer wasn't sent.
        let event = match arrival {
            Arrival::Op(incoming) => {
                let (workflow, bodies) = (self.workflow(), self.bodies);
                // The agent's now function's body is evaluated as its call arrives.
                let (request, unanswered) = request(incoming, |function, arguments| {
                    let mut now = workflow.now_functions_of(id.role());
                    let (_, now) = now.find(|(_, now)| now.name == *function)?;
                    Some(call::now::evaluate(now, arguments, bodies))
                });
                Some(self.op(Asked::Agent(id, unanswered), request))
            }
            Arrival::Refused(refused) => {
                self.refuse(io, id, refused, core.agent_due(id))?;
                None
            }
            // With no agent, the core waits on neither. The loop tears the agent down in the batch
            // that hears of its end, or of its command's failed start, so whatever of the
            // session's it takes after that batch comes here, such as its command's exit as the
            // teardown's freeze kills it, its end as the teardown drops it, or the exit of a
            // command whose start failed at its deadline as the sandbox said it started.
            Arrival::Exited { process, status } => {
                (self.agents.get_mut(&id)).and_then(|agent| agent.exited(id, process, status))
            }
            Arrival::Ended { reason } => {
                (self.agents.get_mut(&id)).and_then(|agent| agent.ended(id, &reason))
            }
            Arrival::ModelRequest(incoming) => {
                self.model_request(live, core, (Caller::Agent(id), at), incoming)?;
                None
            }
        };
        Ok(event.map(|event| (at, event)))
    }

    // Takes what reached the loop before the core's end, so that each op that still arrived is
    // refused by the core and recorded.
    pub(crate) fn drain(
        &mut self,
        io: &mut Io<'r, 's, H>,
        core: &mut Core<'_>,
    ) -> Result<(), String> {
        // Its teardown has dropped the agent's session, unless the run was abandoned first, and
        // this drops it then, its sandbox left to the sweep: the replies to its ops that still
        // arrived are noted as unsent.
        for agent in std::mem::take(&mut self.agents).into_values() {
            if let Some(session) = agent.session {
                Sessions::drop_session(self, io, session);
            }
        }
        while let Some(input) = self.inputs.pop_front() {
            let Some((at, event)) = self.arrived(io, core, input, false)? else {
                continue;
            };
            let actions = core.handle(at, event.clone());
            self.log.event(self.workflow(), at, &event, &actions)?;
            // A line whose actions the runtime carries out next lasts before it does.
            if !actions.is_empty() {
                self.synced(io)?;
            }
            for action in actions {
                match action {
                    Action::Reply { op, reply } => self.reply(io, op, reply, core)?,
                    // A type excluding these actions here would split the core's actions by the
                    // run's phase.
                    other @ (Action::StartAgents
                    | Action::StartFinalCall { .. }
                    | Action::StartCall { .. }
                    | Action::StopCall { .. }
                    | Action::StartLease { .. }
                    | Action::RunCommand { .. }
                    | Action::Transfer { .. }
                    | Action::CloseLease { .. }
                    | Action::TearDownShared { .. }
                    | Action::TearDownAgent { .. }
                    | Action::Finish { .. }
                    | Action::Abandon) => {
                        unreachable!(
                            "the core asked for {other:?} once it had ended the run, though \
                             after `Finish` or `Abandon` it only refuses the ops that still \
                             arrive, as `Action::Finish` and `Action::Abandon` say"
                        )
                    }
                }
            }
        }
        self.failure.take().map_or(Ok(()), Err)
    }

    pub(crate) fn settle(&mut self) -> Result<(), String> {
        let mut failures = Vec::new();
        while let Some((_, input)) = self.inputs.pop_front() {
            let logged = match input {
                Input::Logged(entry) => self.log.entry(entry),
                Input::Child(Report::Undeleted(reason)) => Err(reason),
                Input::Agent(..)
                | Input::Shared(..)
                | Input::Unstarted(..)
                | Input::Core(_)
                | Input::Session(_)
                | Input::Start(_)
                | Input::Delete(_)
                | Input::Dispose(_)
                | Input::Edge(_)
                | Input::Exited
                | Input::Child(
                    Report::Event(..)
                    | Report::CallEnded { .. }
                    | Report::Op { .. }
                    | Report::Model { .. },
                ) => Ok(()),
            };
            failures.extend(logged.err());
        }
        failures.extend(self.failure.take());
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }

    pub(crate) fn sweep(&mut self, io: &mut Io<'r, 's, H>) -> Result<(), String> {
        let swept = self.sweep_on(io, |_| true);
        failed_also(swept, Ok(()))
    }
}
