//! A work phase owns one sandbox per work-lifetime target. Its worker lends the
//! session to each admitted body in turn, then collects and deletes the sandbox on closure.

use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use decision::{CallResult, Event, JobId};
use gateway::Arrival;
use schema::sandbox::{log_dir, out_dir};
use schema::workflow::TargetId;
use stdx::Context;
use stdx::io::Events;
use stdx::time::Clock as _;

use crate::call::job;
use crate::child::{Child, End, Message, Report};
use crate::sandbox::{Sandboxes, Started, Which, collect};
use crate::{Entry, Host, Inbox, Input, Runtime};

type Call<'r, H> = (job::Call<'r>, Child<'r, H, crate::relay::Order>);

pub(super) struct Resident<'r, H: Host> {
    orders: Option<Sender<Call<'r, H>>>,
    failed: Option<String>,
    active: Option<(JobId, Sender<Message<crate::relay::Order>>)>,
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
        child: Child<'r, H, crate::relay::Order>,
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

impl<'r, H: Host + 'r> Runtime<'r, '_, H> {
    pub(super) fn start_residents(&mut self) -> Result<(), String> {
        for target in self.workflow().work_targets() {
            let which = Which::Shared(target);
            let by = self.placed.startup(target.role());
            let (resident, rx) = Resident::open();
            self.residents.insert(target, resident);
            let inbox = self.events.clone();
            let clock = self.clock.clone();
            let sandboxes = self.sandboxes;
            let bodies = self.bodies;
            let dir = self.dir.join(which.record_dir(self.workflow()));
            let collect_end = self.teardown_end();
            let to_loop = self.events.clone();
            let arrivals = Box::new(move |arrival| to_loop.send(Input::Shared(target, arrival)));
            let thread = (&*which.name().to_string(), Some(bodies.stack_bytes()));
            self.spawn(thread, move || {
                let started = sandboxes.start(which, None, by, arrivals);
                let mut sandbox = match started {
                    Ok(sandbox) => {
                        let at = clock.now();
                        inbox.send(Input::Logged(Entry::SharedStarted { target, at }));
                        Some(sandbox)
                    }
                    Err(failure) => {
                        if let Err(reason) = failure.dispose(sandboxes) {
                            inbox.send(Input::Child(Report::Undeleted(reason)));
                        }
                        None
                    }
                };
                serve(rx, bodies, sandbox.as_mut());
                let torn_down = Report::Event(clock.now(), Event::SharedTornDown { target });
                match sandbox {
                    Some(sandbox) => {
                        let by = collect_end;
                        let (deleted, edge) = tear_down(sandbox, sandboxes, &dir, by, &inbox);
                        let report = match deleted {
                            Ok(()) => torn_down,
                            Err(reason) => Report::Undeleted(reason),
                        };
                        edge.send(Input::Child(report));
                    }
                    None => inbox.send(Input::Child(torn_down)),
                }
            })?;
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
        // The shared sandbox outlives the call, which holds none of its own.
        child.close(end, || (Ok(()), None));
    }
}

fn tear_down(
    mut sandbox: Started,
    sandboxes: &Sandboxes<'_>,
    dir: &Path,
    by: Duration,
    inbox: &Events<Input>,
) -> (Result<(), String>, stdx::io::Edge<Input>) {
    let note = |text| inbox.send(Input::Logged(Entry::Note(text)));
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
    sandboxes.delete(sandbox)
}
