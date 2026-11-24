//! A work phase owns one sandbox per work-lifetime target, on the loop. It lends the session to
//! each call's body in turn, then collects and deletes the sandbox once the phase closes it and
//! its last call has ended.

use std::collections::VecDeque;
use std::path::PathBuf;

use decision::{CallResult, Event, JobId};
use gateway::{Answer, Arrival, SessionId, Sessions, Takes};
use provider::SandboxId;
use schema::sandbox::{log_dir, out_dir};
use schema::workflow::TargetId;

use crate::body::Stdin;
use crate::child::{End, collect, uncollected};
use crate::log::Entry;
use crate::sandbox::Which;
use crate::{Host, Input, Io, Purpose, Runtime};

pub(super) struct Resident {
    dir: PathBuf,
    started: Option<(SandboxId, SessionId)>,
    step: Step,
    // The calls that wait for their turn, behind the one whose body runs.
    calls: VecDeque<JobId>,
    active: Option<JobId>,
    /// The standard input that the calls' bodies share, but while one runs.
    pub(super) stdin: Option<Stdin>,
    failed: Option<String>,
    closed: bool,
    // Why its sandbox's session ended, which a call's body on it asks.
    pub(super) session_end: Option<String>,
}

/// A shared role's step under way, which names its ask's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    Starting,
    Serving,
    Freezing,
    // Its `out`, then its `log`.
    Collecting(u8),
    Deleting,
}

impl Resident {
    /// Whether its sandbox's session is `session`.
    pub(super) fn holds(&self, session: SessionId) -> bool {
        matches!(self.started, Some((_, of)) if of == session)
    }

    /// Whether it is under way: until its sandbox's delete begins.
    pub(super) fn busy(&self) -> bool {
        self.step != Step::Deleting
    }
}

impl<'r, 's, H: Host + 'r> Runtime<'r, 's, H> {
    pub(super) fn start_residents(&mut self, io: &mut Io<'r, 's, H>) {
        for target in self.workflow().work_targets() {
            let which = Which::Shared(target);
            let resident = Resident {
                dir: self.dir.join(which.record_dir(self.workflow())),
                started: None,
                step: Step::Starting,
                calls: VecDeque::new(),
                active: None,
                stdin: Some(Stdin::default()),
                failed: None,
                closed: false,
                session_end: None,
            };
            self.residents.insert(target, resident);
            let takes = Takes {
                ops: None,
                models: self.workflow().model(target.role()).is_some(),
            };
            let by = self.placed.startup(target.role());
            self.begin_start(io, which, (by, takes));
        }
    }

    // The shared role of `target`, which the run keeps until its end.
    fn resident(&mut self, target: TargetId) -> &mut Resident {
        let Some(resident) = self.residents.get_mut(&target) else {
            unreachable!("a work-lifetime target's shared role was acted on before the work began")
        };
        resident
    }

    /// Takes the shared role's sandbox, started and set up, whose calls run next.
    pub(super) fn resident_ready(
        &mut self,
        io: &mut Io<'r, 's, H>,
        target: TargetId,
        started: (SandboxId, SessionId),
    ) {
        let resident = self.resident(target);
        resident.started = Some(started);
        resident.step = Step::Serving;
        let at = self.at;
        self.logged(Entry::SharedStarted { target, at });
        self.resident_next(io, target);
    }

    /// Takes the end of the shared role's start that failed, whose calls fail from now on.
    pub(super) fn resident_unstarted(&mut self, io: &mut Io<'r, 's, H>, target: TargetId) {
        self.resident(target).step = Step::Serving;
        self.resident_next(io, target);
    }

    /// Queues call `job` on the shared role, which runs it in its turn.
    pub(super) fn resident_call(&mut self, io: &mut Io<'r, 's, H>, target: TargetId, job: JobId) {
        let resident = self.resident(target);
        resident.calls.push_back(job);
        self.resident_next(io, target);
    }

    /// Takes the end of the call on the shared role, with the standard input as it left it.
    pub(super) fn resident_done(
        &mut self,
        io: &mut Io<'r, 's, H>,
        target: TargetId,
        stdin: Option<Stdin>,
    ) {
        let resident = self.resident(target);
        resident.active = None;
        if stdin.is_some() {
            resident.stdin = stdin;
        }
        self.resident_next(io, target);
    }

    /// The end of a call that ran on `target`'s shared role, which its failure, if it has failed,
    /// makes its own.
    pub(super) fn resident_end(&self, target: TargetId, end: &mut Result<CallResult, String>) {
        if let Some(failed) =
            (self.residents.get(&target)).and_then(|resident| resident.failed.as_ref())
        {
            *end = Err(failed.clone());
        }
    }

    // Runs the next call in its turn, once the shared role serves and no call's body runs; ends
    // the shared role once the phase has closed it and no call is left.
    fn resident_next(&mut self, io: &mut Io<'r, 's, H>, target: TargetId) {
        loop {
            let Some(resident) = self.residents.get_mut(&target) else {
                return;
            };
            if resident.step != Step::Serving || resident.active.is_some() {
                return;
            }
            let Some(job) = resident.calls.pop_front() else {
                if resident.closed {
                    self.tear_down_resident(io, target);
                }
                return;
            };
            if let Some((_, session)) = resident.started {
                resident.active = Some(job);
                return self.work(io, job, session);
            }
            let unstarted = Err("the shared sandbox never started".to_owned());
            self.end_unworked(io, job, End::Call(job, unstarted));
        }
    }

    /// Takes what the shared role's session handed over.
    pub(super) fn resident_arrived(
        &mut self,
        io: &mut Io<'r, 's, H>,
        target: TargetId,
        arrival: Arrival,
    ) {
        let resident = self.resident(target);
        // Deleting the sandbox ends its session.
        let closed = resident.closed;
        let reason = match arrival {
            Arrival::Ended { reason } => {
                resident.session_end = Some(reason.clone());
                let reason = format!("the shared role's session ended: {reason}");
                if let Some(active) = resident.active {
                    self.call_ends(io, active, reason.clone());
                }
                reason
            }
            arrival @ Arrival::Exited { .. } => {
                if let Some(active) = resident.active.filter(|_| !closed) {
                    self.call_arrived(io, active, arrival);
                }
                return;
            }
            // The gateway refuses these messages on a session with neither ops nor a model.
            Arrival::Op(_) | Arrival::ModelRequest(_) | Arrival::Refused(_) => {
                "the shared role's session sent an unauthorized request".to_owned()
            }
        };
        if !closed {
            self.input(Input::Core(Event::SharedFailed { target, reason }));
        }
    }

    /// The event of the shared role's failure for `reason`, the first time it fails: its call
    /// that runs, if one does, fails with it.
    pub(super) fn resident_fail(
        &mut self,
        io: &mut Io<'r, 's, H>,
        target: TargetId,
        reason: String,
    ) -> Option<Event> {
        let resident = self.residents.get_mut(&target)?;
        if resident.failed.is_some() {
            return None;
        }
        resident.failed = Some(reason.clone());
        if let Some(active) = resident.active {
            self.call_ends(io, active, reason.clone());
        }
        Some(Event::SharedFailed { target, reason })
    }

    /// Takes the end of the work phase, which ends the shared role once its calls have ended.
    pub(super) fn close_resident(&mut self, io: &mut Io<'r, 's, H>, target: TargetId) {
        let resident = self.resident(target);
        if resident.closed {
            return;
        }
        resident.closed = true;
        // The body's driver stops forwarding command exits while it awaits a killed client.
        if let Some(active) = resident.active {
            self.call_ends(io, active, "the work phase ended".to_owned());
        }
        self.resident_next(io, target);
    }

    // Freezes the shared role's sandbox, then collects it, then deletes it.
    fn tear_down_resident(&mut self, io: &mut Io<'r, 's, H>, target: TargetId) {
        let resident = self.resident(target);
        let Some((_, session)) = resident.started else {
            resident.step = Step::Deleting;
            let after = End::Event(Event::SharedTornDown { target });
            return self.close_child(io, (Which::Shared(target), None), after);
        };
        resident.step = Step::Freezing;
        let token = self.token(Purpose::Shared(target, Step::Freezing));
        let by = self.teardown_end();
        Sessions::ask(self, io, session, gateway::Ask::Freeze, (by, token));
    }

    /// Takes the answer to the shared role's step.
    pub(super) fn resident_answered(
        &mut self,
        io: &mut Io<'r, 's, H>,
        (target, step): (TargetId, Step),
        answer: Answer,
    ) {
        let part = match (step, answer) {
            (Step::Freezing, Answer::Done(frozen)) => {
                if let Err(error) = frozen {
                    self.note(&format!("freezing the shared sandbox failed: {error}"));
                }
                0
            }
            (Step::Collecting(part), Answer::Collected(collected)) => {
                if let Err(error) = collected {
                    let dir = [out_dir(), log_dir()][usize::from(part)].clone();
                    let error = uncollected(&dir, &error);
                    self.note(&format!(
                        "the shared sandbox's {dir} is incomplete: {error}"
                    ));
                }
                part + 1
            }
            (step, answer) => unreachable!("{answer:?} answered a shared role's {step:?}"),
        };
        let resident = self.resident(target);
        let Some((_, session)) = resident.started else {
            unreachable!("a shared role collects only from its sandbox")
        };
        if let Some(dir) = [out_dir(), log_dir()].get(usize::from(part)) {
            resident.step = Step::Collecting(part);
            let ask = collect(dir, &resident.dir.join(dir.as_str()));
            let token = self.token(Purpose::Shared(target, Step::Collecting(part)));
            let by = self.teardown_end();
            Sessions::ask(self, io, session, ask, (by, token));
            return;
        }
        resident.step = Step::Deleting;
        let child = (Which::Shared(target), resident.started.take());
        self.close_child(io, child, End::Event(Event::SharedTornDown { target }));
    }
}
