//! The run's model routes, as its loop serves them: a route for each caller whose role has one,
//! from the caller's start until the core stops allowing its requests, and a thread for each
//! request.

use std::sync::Arc;
use std::thread::{self, Scope};
use std::time::Duration;

use decision::{Caller, Core, Outcome, WorkEnd};
use gateway::IncomingModelRequest;
use route::{Admission, Answer, Bar};
use schema::ir::{Api, Name};
use schema::workflow::{AgentId, RoleId};

use crate::budget::Budget;
use crate::host::left;
use crate::log::{Entry, ModelAnswer};
use crate::model::{Exchange, Served};
use crate::{Clock, Host, Inbox, Input, Runtime, Stopped};

impl<'r, H: Host + 'r> Runtime<'r, H> {
    pub(crate) fn model_request<'s>(
        &mut self,
        scope: Option<&'s Scope<'s, 'r>>,
        core: &Core<'_>,
        (caller, at): (Caller, Duration),
        incoming: IncomingModelRequest,
    ) -> Result<(), String> {
        let number = self.next_model_request;
        self.next_model_request += 1;
        // A call's start and request may both arrive after the run ends, with no route installed.
        let Some(scope) = scope else {
            drop(incoming);
            let note = format!("model request {number} wasn't answered: the run has ended");
            return self.log.note(&note);
        };
        // No type excludes a model request here, since one `Arrival` serves sessions with model
        // requests and without.
        let Some(served) = self.served.get(&caller).map(Arc::clone) else {
            unreachable!(
                "a model request arrived from {caller:?} with no route, though only a role with \
                 a route takes model requests, the agent's is served before its sandbox starts, \
                 and a call reports its start before relaying its first request"
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
            Caller::Job(job) => self.call(job).collect,
        };
        let reports = self.inbox.sender();
        let clock = self.clock.clone();
        // The time left whole, not at most `Clock::look`, as the route bounds the exchange by it
        // as the exchange begins.
        let by = {
            let clock = clock.clone();
            move || left(&clock, by)
        };
        let exchange = Exchange::new(self.dir, number, clock.clone());
        // A request stays in flight until its thread writes its response's end, the thread's last
        // step, so the session's bound on the requests in flight, `MAX_MODEL_REQUESTS_IN_FLIGHT`,
        // bounds these threads too, but for those writing that end.
        let spawned = thread::Builder::new()
            .name(format!("model-{number}"))
            .spawn_scoped(scope, move || {
                // The loop's inbox outlives every request's thread, which the run's end waits for.
                // Both reports come before the answer's end, and so before what the caller does
                // next. Each time is read here, as it happens, since the loop writes the record
                // later, by however long it is busy.
                let forwarding = || {
                    let started = Entry::ModelRequestStarted((caller, number), clock.now());
                    let _ = reports.send(Input::Logged(started));
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
                    let _ = reports.send(Input::Logged(answered));
                };
                served.serve(
                    (number, &exchange),
                    incoming,
                    admission,
                    by,
                    forwarding,
                    answered,
                );
                if let Some(failed) = exchange.finish() {
                    let note = format!("model request {number} wasn't kept whole: {failed}");
                    let _ = reports.send(Input::Logged(Entry::Note(note)));
                }
            });
        match spawned {
            Ok(_) => Ok(()),
            Err(error) => {
                let note = format!(
                    "model request {number} wasn't answered: its thread didn't start: {error}"
                );
                self.log.note(&note)
            }
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
