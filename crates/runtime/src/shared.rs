//! A work phase owns one sandbox per work-lifetime target. Its worker lends the
//! session to each admitted body in turn, then collects and deletes the sandbox on closure.

use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, Scope};
use std::time::Duration;

use decision::{CallResult, Event, JobId};
use gateway::Arrival;
use schema::sandbox::op::OpReply;
use schema::sandbox::{log_dir, out_dir};
use schema::workflow::TargetId;
use stdx::Context;

use crate::agent::Ready;
use crate::call::job;
use crate::child::{Child, End, Message, Report};
use crate::sandbox::{Sandboxes, Started, Which, collect};
use crate::{Clock, Entry, Host, Inbox, Input, Runtime, deadline};

type Call<'r, H> = (job::Call<'r>, Child<'r, H, OpReply>);

pub(super) struct Resident<'r, H: Host> {
    orders: Option<Sender<Call<'r, H>>>,
    failed: Option<String>,
    active: Option<(JobId, Sender<Message<OpReply>>)>,
}

impl<'r, H: Host> Resident<'r, H> {
    fn open() -> (Self, Receiver<Call<'r, H>>) {
        let (tx, rx) = mpsc::channel();
        let resident = Self {
            orders: Some(tx),
            failed: None,
            active: None,
        };
        (resident, rx)
    }

    pub(super) fn call(
        &mut self,
        call: job::Call<'r>,
        child: Child<'r, H, OpReply>,
    ) -> Result<(), String> {
        let active = (call.job, child.inbox.sender());
        let sender = self.orders.as_ref().ok_or("the shared worker is closed")?;
        sender
            .send((call, child))
            .context("sending a call to its shared worker")?;
        self.active = Some(active);
        Ok(())
    }

    pub(super) fn finish(&mut self, job: JobId, end: &mut Result<CallResult, String>) {
        if self.active.as_ref().is_some_and(|(id, _)| *id == job) {
            self.active = None;
            if let Some(reason) = &self.failed {
                *end = Err(reason.clone());
            }
        }
    }

    pub(super) fn arrival(&mut self, target: TargetId, arrival: Arrival) -> Option<Event> {
        // Deleting the sandbox ends its session.
        self.orders.as_ref()?;
        let reason = match arrival {
            Arrival::Ended { reason } => format!("the shared role's session ended: {reason}"),
            arrival @ Arrival::Exited { .. } => {
                if let Some((_, sender)) = &self.active {
                    let _ = sender.send(Message::Session(arrival));
                }
                return None;
            }
            // The gateway refuses these messages on a session with neither ops nor a model.
            Arrival::Op(_) | Arrival::ModelRequest(_) | Arrival::Refused(_) => {
                "the shared role's session sent an unauthorized request".to_owned()
            }
        };
        self.fail(target, reason)
    }

    pub(super) fn fail(&mut self, target: TargetId, reason: String) -> Option<Event> {
        if self.failed.is_some() {
            return None;
        }
        if let Some((_, sender)) = &self.active {
            let _ = sender.send(Message::Session(Arrival::Ended {
                reason: reason.clone(),
            }));
        }
        self.failed = Some(reason.clone());
        Some(Event::SharedFailed { target, reason })
    }

    pub(super) fn close(&mut self) {
        self.orders.take();
        if let Some((_, sender)) = &self.active {
            // The driver can stop forwarding command exits while the body awaits a killed client.
            let _ = sender.send(Message::Session(Arrival::Ended {
                reason: "the work phase ended".to_owned(),
            }));
        }
    }
}

impl<'r, H: Host + 'r> Runtime<'r, H> {
    pub(super) fn start_residents<'s>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
        ready: &Sender<(Duration, Ready)>,
    ) -> Result<(), String> {
        for target in self.workflow().work_targets() {
            let which = Which::Shared(target);
            let by = deadline(&self.clock, self.placed.startup(target.role()));
            let hard_end = self.unix_hard_end;
            let (resident, rx) = Resident::open();
            self.residents.insert(target, resident);
            let inbox = self.inbox.sender();
            let clock = self.clock.clone();
            let observed = clock.clone();
            let ready = ready.clone();
            let sandboxes = self.sandboxes;
            let bodies = self.bodies;
            let dir = self.dir.join(which.record_dir(self.workflow()));
            let collect_end = self.teardown_end();
            thread::Builder::new()
                .name(which.name().to_string())
                .stack_size(bodies.stack_bytes())
                .spawn_scoped(scope, move || {
                    let arrivals = move |arrival| Input::Shared(target, observed.now(), arrival);
                    let started =
                        sandboxes.start(which, None, by, hard_end, inbox.clone(), arrivals);
                    let mut sandbox = match started {
                        Ok(sandbox) => {
                            let at = clock.now();
                            let _ = inbox.send(Input::Logged(Entry::SharedStarted { target, at }));
                            let _ = ready.send((at, Ready::Shared(target, Ok(()))));
                            Some(sandbox)
                        }
                        Err(failure) => {
                            let _ = ready.send((
                                clock.now(),
                                Ready::Shared(target, Err(failure.reason().to_owned())),
                            ));
                            if let Err(reason) = failure.dispose(sandboxes) {
                                let _ = inbox.send(Input::Child(Report::Undeleted(reason)));
                            }
                            None
                        }
                    };
                    drop(ready);
                    serve(rx, bodies, sandbox.as_mut());
                    if let Some(sandbox) = sandbox {
                        let by = deadline(&clock, collect_end);
                        if let Err(reason) = tear_down(sandbox, sandboxes, &dir, by, &inbox) {
                            let _ = inbox.send(Input::Child(Report::Undeleted(reason)));
                            return;
                        }
                    }
                    let _ = inbox.send(Input::Child(Report::Event(
                        clock.now(),
                        Event::SharedTornDown { target },
                    )));
                })
                .context("starting the shared role's worker")?;
        }
        Ok(())
    }
}

fn serve<H: Host>(
    orders: Receiver<Call<'_, H>>,
    bodies: &dyn crate::Bodies,
    mut sandbox: Option<&mut Started>,
) {
    let mut stdin = crate::body::Stdin::default();
    for (call, mut child) in orders {
        let end = match sandbox.as_deref_mut() {
            Some(started) => {
                job::evaluate(call, bodies, &mut child, &mut started.session, &mut stdin)
            }
            None => End::Call(call.job, Err("the shared sandbox never started".to_owned())),
        };
        child.report(end.report(child.clock.now()));
    }
}

fn tear_down(
    mut sandbox: Started,
    sandboxes: &Sandboxes<'_, impl Host>,
    dir: &Path,
    by: stdx::Deadline,
    inbox: &Sender<Input>,
) -> Result<(), String> {
    let note = |text| {
        let _ = inbox.send(Input::Logged(Entry::Note(text)));
    };
    if let Err(error) = sandbox.session.freeze(by) {
        note(format!("freezing the shared sandbox failed: {error}"));
    }
    for part in [out_dir(), log_dir()] {
        if let Err(error) = collect(&mut sandbox.session, &part, dir, by) {
            note(format!(
                "the shared sandbox's {part} is incomplete: {error}"
            ));
        }
    }
    let (entries, deleted) = sandboxes.delete(sandbox);
    for entry in entries {
        let _ = inbox.send(Input::Logged(entry));
    }
    deleted
}
