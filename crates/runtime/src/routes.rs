//! The run's model routes, as its loop serves them: a route for each caller whose role has one,
//! from the caller's start until the core stops allowing its requests, and a thread for each
//! request.

use std::sync::Arc;
use std::thread::{self, Scope};
use std::time::Duration;

use decision::{Caller, Core, Outcome};
use gateway::IncomingModelRequest;
use route::Admission;
use schema::ir::{Api, Name};
use schema::workflow::RoleId;

use crate::budget::Budget;
use crate::host::deadline;
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
        // No type excludes a model request here, since one `Arrival` serves sessions with model
        // requests and without.
        let Some(served) = self.served.get(&caller).map(Arc::clone) else {
            unreachable!(
                "a model request arrived from {caller:?}, though a session takes them only if its \
                 role has a route (`Sandboxes::start`), whose sandbox's start serves it, and one \
                 breaks the protocol otherwise, which ends the session"
            )
        };
        // `scope` is none once the run has ended, and no thread starts then.
        let Some(scope) = scope else {
            drop(incoming);
            let note = format!("model request {number} wasn't answered: the run has ended");
            return self.log.note(&note);
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
            Caller::Agent => self.teardown_end(),
            Caller::Job(job) => self.call(job).collect,
        };
        let by = deadline(&self.clock, by);
        let reports = self.inbox.sender();
        let clock = self.clock.clone();
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
        }
    }

    fn role(&mut self, caller: Caller) -> RoleId {
        match caller {
            Caller::Agent => self.workflow().agent().role,
            Caller::Job(job) => self.call(job).role,
        }
    }

    pub(crate) fn serve(&mut self, caller: Caller, role: RoleId) {
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

    // Fails the run for a route whose upstream refused its key, unless the refusal came after the
    // selection was locked on a route that served before it, and for a failure of the agent's last
    // request.
    pub(crate) fn routes_held(&self, outcome: &Result<Outcome, Stopped>) -> Result<(), String> {
        // A refusal heard after the lock can't change the result: the agent's or a call's whose
        // answer the agent can't act on. The final call starts after the lock, so a
        // refusal on its route always fails the run.
        for (caller, served) in &self.served {
            if let Some(name) = served.key_refused_by()
                && !self.held_at_lock.contains(caller)
            {
                let route = match caller {
                    Caller::Agent => "the agent's".to_owned(),
                    Caller::Job(job) => format!("job {}'s", job.0),
                };
                return Err(format!(
                    "the upstream `{name}` refused its key, with a 401, which ended {route} \
                     model route: give it a key that it takes, and run again"
                ));
            }
        }
        match self.served.get(&Caller::Agent) {
            Some(agent) => agent.failed_last(outcome),
            None => Ok(()),
        }
    }
}
