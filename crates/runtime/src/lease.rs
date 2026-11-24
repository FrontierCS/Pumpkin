//! Leases, each a child of the run ([`child`](super::child)) on the loop: once its runner is
//! ready, a lease runs the commands and transfers the core orders ([`Order`]), one at a time, and
//! the core is told how each ended. A command runs in the runner's `cwd` with its `env`, its
//! output in a directory of its own below [`log_dir()`], `log/<c>` for its `c`th, which is
//! collected into the same path in the lease's directory of the record once the command ends. A
//! command that runs past its timeout, if it has one, is killed alone. A transfer collects the
//! tree from one sandbox into the lease's directory, [`STAGING`], puts it into the other, and
//! removes it; the core's stop cuts it short. The core's stop closes the lease once its step under
//! way has ended, and so does its runner's failure: its runner is frozen, and its `out`, and the
//! log of a command that still ran, are collected.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::num::NonZeroU64;
use std::path::Path;
use std::time::Duration;

use decision::{Direction, Event, LeaseId};
use gateway::{Answer, Arrival, Ask, Process, Put, SessionId, Sessions};
use schema::ir::{AbsolutePath, Argv};
use schema::sandbox::op::{self, CommandEnd, OUTPUT_TAIL_BYTES, TransferEnd};
use schema::sandbox::tree::Limits;
use schema::sandbox::{
    Entry, ExitStatus, Launch, RelativePath, Root, command_env, dir_mode, log_dir, out_dir,
    pumpkin_dir,
};
use schema::workflow::Runner;

use super::child::{Child, End, collect, uncollected};
use super::sandbox::ENTRIES;
use super::{Host, Input, Io, Purpose, Runtime};

/// The directory of a lease's record that holds the tree a transfer moves, while it does.
pub(super) const STAGING: &str = "transfer";

pub(super) enum Order {
    Run(Run),
    Transfer(Transfer),
}

/// The core's order to copy a tree between the holder's sandbox and a lease's runner, as its
/// `number`th command or transfer.
pub(super) struct Transfer {
    pub(super) number: u64,
    pub(super) direction: Direction,
    /// The most bytes of files it may move.
    pub(super) max_bytes: NonZeroU64,
    /// The holder's sandbox's session.
    pub(super) holder: SessionId,
}

/// The core's order to run a command in a lease's runner, its `command`th.
pub(super) struct Run {
    pub(super) command: u64,
    pub(super) argv: Argv,
    /// The longest it may run, if it has a bound of its own.
    pub(super) timeout: Option<Duration>,
    pub(super) required_output: bool,
}

/// A lease, from its start until its runner is deleted.
pub(super) struct Lease<'r> {
    child: Child,
    runner: &'r Runner,
    /// By when what the lease puts in its sandbox, each command's start and each kill are done,
    /// as a time since the run started.
    start: Duration,
    step: Step,
    run: Option<Run>,
    running: Option<Running>,
    transfer: Option<Moving>,
    stopped: bool,
    /// Why the runner failed, which ends the lease once its step under way has.
    failure: Option<String>,
}

/// A lease's step under way, which names its ask's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    Starting,
    Idle,
    // A command's directory, then its start.
    Making,
    Launching,
    Running,
    Killing,
    // The output of a command that has ended.
    Outputs,
    // A transfer's tree, collected into the record, then put out of it.
    Collecting,
    Putting,
    Freezing,
    // The output of the command that still ran, then `out`.
    Closing(u8),
    Deleting,
}

struct Running {
    command: u64,
    process: Process,
    /// When it is to be killed, since the run started, until it has been.
    kill_at: Option<Duration>,
    timed_out: bool,
    required_output: bool,
    /// Its exit, held while its kill is answered.
    exited: Option<ExitStatus>,
}

/// A transfer under way: the core's order, the tree's size once collected, its ask's token, and
/// whether the core's stop cut it short.
struct Moving {
    order: Transfer,
    moved: (u64, u64),
    token: u64,
    cut: bool,
}

impl Lease<'_> {
    /// Whether its runner's session is `session`.
    pub(super) fn holds(&self, session: SessionId) -> bool {
        matches!(self.child.started, Some((_, of)) if of == session)
    }

    /// Whether it is under way: until its runner's delete begins.
    pub(super) fn busy(&self) -> bool {
        self.step != Step::Deleting
    }

    // When its command is to be killed, if it runs and hasn't been.
    fn kill_at(&self) -> Option<Duration> {
        let running = self
            .running
            .as_ref()
            .filter(|_| self.step == Step::Running)?;
        running.kill_at
    }
}

impl<'r, 's, H: Host + 'r> Runtime<'r, 's, H> {
    /// Opens lease `id`, whose runner `child` the loop starts by its startup, unless that has
    /// passed, which fails the lease at once.
    pub(super) fn open_lease(
        &mut self,
        io: &mut Io<'r, 's, H>,
        id: LeaseId,
        (child, runner, start): (Child, &'r Runner, Duration),
    ) {
        let which = child.which;
        let startup = child.deadlines.startup;
        let lease = Lease {
            child,
            runner,
            start,
            step: Step::Starting,
            run: None,
            running: None,
            transfer: None,
            stopped: false,
            failure: None,
        };
        self.leases.insert(id, lease);
        self.start_child(io, which, (startup, None));
    }

    /// Takes why the lease's runner didn't start, which fails the lease.
    pub(super) fn lease_unstarted(&mut self, io: &mut Io<'r, 's, H>, id: LeaseId, reason: String) {
        self.lease_delete(io, id, Event::LeaseFailed { lease: id, reason });
    }

    /// Takes the lease's runner, started and set up.
    pub(super) fn lease_ready(
        &mut self,
        io: &mut Io<'r, 's, H>,
        id: LeaseId,
        started: (provider::SandboxId, SessionId),
    ) {
        let lease = self.lease(id);
        lease.child.started = Some(started);
        if lease.stopped {
            return self.lease_delete(io, id, Event::LeaseEnded { lease: id });
        }
        if let Err(error) = fs::create_dir(lease.child.dir.join(log_dir().as_str())) {
            let reason = format!("the lease's log can't be made in the record: {error}");
            return self.lease_delete(io, id, Event::LeaseFailed { lease: id, reason });
        }
        lease.step = Step::Idle;
        self.input(Input::Core(Event::LeaseReady { lease: id }));
    }

    /// Takes the core's order, which comes only while the lease is idle, as the core orders one
    /// command or transfer at a time, but once the lease is closing, which drops it.
    pub(super) fn lease_order(&mut self, io: &mut Io<'r, 's, H>, id: LeaseId, order: Order) {
        let lease = self.lease(id);
        if lease.stopped || lease.failure.is_some() || lease.step != Step::Idle {
            return;
        }
        let Some((_, session)) = lease.child.started.clone() else {
            unreachable!("an idle lease holds its runner")
        };
        match order {
            Order::Run(run) => {
                let dir = command_dir(run.command);
                let put = Ask::Put(pumpkin_dir(), Root::Kept, vec![Put::Dir(dir, dir_mode())]);
                lease.run = Some(run);
                self.lease_ask(io, (id, Step::Making), session, put);
            }
            Order::Transfer(order) => {
                let ends = Ends::of(&order);
                let source = if ends.into_runner {
                    order.holder
                } else {
                    session
                };
                let staging = lease.child.dir.join(STAGING);
                let limits = Limits {
                    bytes: order.max_bytes.get(),
                    entries: ENTRIES,
                };
                let collect = Ask::Collect(ends.from.clone(), staging, limits);
                lease.transfer = Some(Moving {
                    order,
                    moved: (0, 0),
                    token: 0,
                    cut: false,
                });
                self.lease_ask(io, (id, Step::Collecting), source, collect);
            }
        }
    }

    /// Takes the core's stop: the lease closes once its step under way has ended, and a
    /// transfer under way is cut short.
    pub(super) fn close_lease(&mut self, io: &mut Io<'r, 's, H>, id: LeaseId) {
        let lease = self.lease(id);
        lease.stopped = true;
        if let (Some(moving), Some((_, runner))) = (&mut lease.transfer, &lease.child.started) {
            moving.cut = true;
            let (token, holder, runner) = (moving.token, moving.order.holder, *runner);
            let on = match (lease.step, Ends::of(&moving.order).into_runner) {
                (Step::Collecting, true) | (Step::Putting, false) => holder,
                _ => runner,
            };
            Sessions::cancel(self, io, on, token);
        }
        self.lease_next(io, id);
    }

    /// Takes what the runner's session handed over: its command's exit, or its end.
    pub(super) fn lease_arrived(&mut self, io: &mut Io<'r, 's, H>, id: LeaseId, arrival: Arrival) {
        let lease = self.lease(id);
        match arrival {
            Arrival::Exited { process, status } => {
                let Some(running) = lease.running.as_mut().filter(|run| run.process == process)
                else {
                    return;
                };
                if lease.step == Step::Killing {
                    running.exited = Some(status);
                } else if lease.step == Step::Running {
                    self.lease_exited(io, id, status);
                }
            }
            Arrival::Ended { reason } => {
                let reason = format!("the runner's session ended: {reason}");
                lease.failure.get_or_insert(reason);
                self.lease_next(io, id);
            }
            // A type for a passive session's arrivals would split the gateway's `pub` `Arrival` by
            // the kind of session.
            Arrival::Op(_) | Arrival::Refused(_) | Arrival::ModelRequest(_) => unreachable!(
                "lease {}'s passive session handed out an op or a model request, though it was \
                 accepted without an op payload limit and without model requests, so either \
                 ends the session instead, and arrives as its end",
                id.0
            ),
        }
    }

    /// The earliest time that a lease's command is to be killed at, as a time since the run
    /// started.
    pub(super) fn kills_due(&self) -> Option<Duration> {
        self.leases.values().filter_map(Lease::kill_at).min()
    }

    /// Kills each lease's command whose timeout has passed. A command that isn't killed runs on
    /// until it exits, or the lease ends.
    pub(super) fn kill_due(&mut self, io: &mut Io<'r, 's, H>) {
        let at = self.at;
        let due: Vec<LeaseId> = (self.leases.iter())
            .filter(|(_, lease)| lease.kill_at().is_some_and(|due| at >= due))
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            let Some(lease) = self.leases.get_mut(&id) else {
                continue;
            };
            let (Some(running), Some((_, session))) = (&mut lease.running, &lease.child.started)
            else {
                continue;
            };
            running.kill_at = None;
            let (process, session) = (running.process, *session);
            self.lease_ask(io, (id, Step::Killing), session, Ask::Kill(process));
        }
    }

    /// Takes the answer to the lease's step.
    pub(super) fn lease_answered(
        &mut self,
        io: &mut Io<'r, 's, H>,
        (id, step): (LeaseId, Step),
        answer: Answer,
    ) {
        let lease = self.lease(id);
        match (step, answer) {
            (Step::Making, Answer::Done(done)) => self.made(io, id, done),
            (Step::Launching, Answer::Started(started)) => self.command_started(io, id, started),
            (Step::Killing, Answer::Done(killed)) => self.killed(io, id, killed),
            (Step::Outputs, Answer::Collected(collected)) => self.outputs(io, id, collected.err()),
            (Step::Collecting, Answer::Collected(collected)) => {
                self.collected_tree(io, id, collected);
            }
            (Step::Putting, Answer::Done(put)) => {
                let Some(moving) = &lease.transfer else {
                    unreachable!("a lease puts a tree only for its transfer")
                };
                let ends = Ends::of(&moving.order);
                let put = put.map(|()| moving.moved).map_err(|error| {
                    let Ends { to, rule, .. } = &ends;
                    if moving.cut && error.kind() == io::ErrorKind::Interrupted {
                        return format!("{to} holds only part of the tree");
                    }
                    let to_where = if ends.into_runner {
                        "in the runner"
                    } else {
                        "here"
                    };
                    format!(
                        "putting the tree at {to} {to_where} failed: {error}: the tree's root \
                         becomes {to}{rule}"
                    )
                });
                self.transferred(io, id, put);
            }
            (Step::Freezing, Answer::Done(frozen)) => match frozen {
                Ok(()) => self.collect_lease(io, id, 0),
                Err(error) => {
                    let reason = (lease.failure.take()).unwrap_or_else(|| {
                        format!("freezing the runner's sandbox failed: {error}")
                    });
                    self.lease_delete(io, id, Event::LeaseFailed { lease: id, reason });
                }
            },
            (Step::Closing(part), Answer::Collected(collected)) => {
                if let Err(error) = collected {
                    let dir = match (part, &lease.running) {
                        (0, Some(running)) => command_dir(running.command),
                        _ => out_dir(),
                    };
                    let error = uncollected(&dir, &error);
                    self.note(&format!("lease {}'s {dir} is incomplete: {error}", id.0));
                }
                self.collect_lease(io, id, part + 1);
            }
            (step, answer) => unreachable!("{answer:?} answered lease {}'s {step:?}", id.0),
        }
    }

    // Takes the making of the command's directory, which its start follows.
    fn made(&mut self, io: &mut Io<'r, 's, H>, id: LeaseId, done: io::Result<()>) {
        let lease = self.lease(id);
        let (Some(run), Some((_, session))) = (&lease.run, &lease.child.started) else {
            unreachable!("a lease makes a command's directory only for its run, on its runner")
        };
        let (dir, session) = (command_dir(run.command), *session);
        let launch = launch(lease.runner, run);
        match done {
            Ok(()) => self.lease_ask(io, (id, Step::Launching), session, Ask::Start(launch)),
            Err(error) => self.unstarted(io, id, format!("making {dir} failed: {error}")),
        }
    }

    // Takes the command's start, which its exit or its kill follows.
    fn command_started(
        &mut self,
        io: &mut Io<'r, 's, H>,
        id: LeaseId,
        started: io::Result<Process>,
    ) {
        let at = self.at;
        let lease = self.lease(id);
        let process = match started {
            Ok(process) => process,
            Err(error) => {
                return self.unstarted(io, id, format!("the command didn't start: {error}"));
            }
        };
        let Some(run) = lease.run.take() else {
            unreachable!("a lease starts a command only for its run")
        };
        lease.step = Step::Running;
        lease.running = Some(Running {
            command: run.command,
            process,
            kill_at: run.timeout.map(|timeout| at.saturating_add(timeout)),
            timed_out: false,
            required_output: run.required_output,
            exited: None,
        });
        self.lease_next(io, id);
    }

    // Takes the kill of the command, whose exit, if it came meanwhile, goes on now.
    fn killed(&mut self, io: &mut Io<'r, 's, H>, id: LeaseId, killed: io::Result<()>) {
        let lease = self.lease(id);
        lease.step = Step::Running;
        let Some(running) = &mut lease.running else {
            unreachable!("a lease kills only the command that runs")
        };
        let exited = running.exited;
        if let Err(error) = killed {
            let reason = format!("command {} wasn't killed: {error}", running.command);
            if running.required_output {
                lease.failure.get_or_insert(reason);
            } else {
                self.note(&reason);
            }
        } else {
            running.timed_out = true;
        }
        match exited {
            Some(status) => self.lease_exited(io, id, status),
            None => self.lease_next(io, id),
        }
    }

    // The lease that the core opened, which the run keeps until its end.
    fn lease(&mut self, id: LeaseId) -> &mut Lease<'r> {
        let Some(lease) = self.leases.get_mut(&id) else {
            unreachable!(
                "lease {} was acted on, but never opened, though the core acts only on a lease it \
                 has opened, and the `StartLease` that opened it, performed before, made it",
                id.0
            )
        };
        lease
    }

    // Asks the lease's step of `session`.
    fn lease_ask(
        &mut self,
        io: &mut Io<'r, 's, H>,
        (id, step): (LeaseId, Step),
        session: SessionId,
        ask: Ask,
    ) {
        let lease = self.lease(id);
        lease.step = step;
        let by = match step {
            Step::Making | Step::Launching | Step::Killing => lease.start,
            _ => lease.child.deadlines.collect,
        };
        let token = self.token(Purpose::Lease(id, step));
        if let Some(moving) = &mut self.lease(id).transfer {
            moving.token = token;
        }
        Sessions::ask(self, io, session, ask, (by, token));
    }

    // Ends the command that didn't start, for `reason`.
    fn unstarted(&mut self, io: &mut Io<'r, 's, H>, id: LeaseId, reason: String) {
        let lease = self.lease(id);
        let Some(run) = lease.run.take() else {
            unreachable!("a lease's command that didn't start was its run's")
        };
        lease.step = Step::Idle;
        let ended = (CommandEnd::Unstarted, String::new(), reason);
        self.input(Input::Core(command_ended(id, run.command, ended)));
        self.lease_next(io, id);
    }

    // Takes the exit of the lease's command, whose output it collects.
    fn lease_exited(&mut self, io: &mut Io<'r, 's, H>, id: LeaseId, status: ExitStatus) {
        let lease = self.lease(id);
        let (Some(running), Some((_, session))) = (&mut lease.running, &lease.child.started) else {
            unreachable!("an exit ends the lease's command that runs, on its runner")
        };
        running.exited = Some(status);
        let dir = command_dir(running.command);
        let ask = collect(&dir, &lease.child.dir.join(dir.as_str()));
        let session = *session;
        self.lease_ask(io, (id, Step::Outputs), session, ask);
    }

    // Takes the collect of the ended command's output, failed for `failed` if it did, and reports
    // how the command ended with the ends of its standard output and error.
    fn outputs(&mut self, io: &mut Io<'r, 's, H>, id: LeaseId, failed: Option<io::Error>) {
        let lease = self.lease(id);
        let Some(ran) = lease.running.take() else {
            unreachable!("a lease collects the output of the command that ran")
        };
        lease.step = Step::Idle;
        let (command, required) = (ran.command, ran.required_output);
        let dir = command_dir(command);
        let collected = lease.child.dir.join(dir.as_str());
        let mut notes = Vec::new();
        let mut failure = None;
        if let Some(error) = failed {
            let error = uncollected(&dir, &error);
            let reason = format!("command {command}'s output is incomplete: {error}");
            if required {
                failure = Some(reason);
            } else {
                notes.push(reason);
            }
        }
        let mut read_tail = |name: &str| match tail(&collected.join(name)) {
            Ok(text) => text,
            Err(error) if !required && error.kind() == io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                let reason = format!("{} can't be read: {error}", collected.join(name).display());
                if required {
                    failure.get_or_insert(reason);
                } else {
                    notes.push(reason);
                }
                String::new()
            }
        };
        let (stdout, stderr) = (read_tail("command.stdout"), read_tail("command.stderr"));
        for note in notes {
            self.note(&note);
        }
        let lease = self.lease(id);
        if let Some(reason) = failure {
            lease.failure.get_or_insert(reason);
            return self.lease_next(io, id);
        }
        let end = match ran.exited {
            _ if ran.timed_out => CommandEnd::TimedOut,
            Some(status) => CommandEnd::Exited(status),
            None => unreachable!("a lease collects a command's output once it has exited"),
        };
        let event = command_ended(id, command, (end, stdout, stderr));
        self.input(Input::Core(event));
        self.lease_next(io, id);
    }

    // Takes the collect of a transfer's tree into the record, which it puts next, unless the
    // core's stop has cut the transfer short.
    fn collected_tree(
        &mut self,
        io: &mut Io<'r, 's, H>,
        id: LeaseId,
        collected: io::Result<Vec<Entry>>,
    ) {
        let lease = self.lease(id);
        let (Some(moving), Some((_, runner))) = (&mut lease.transfer, &lease.child.started) else {
            unreachable!("a lease collects a tree only for its transfer, on its runner")
        };
        let ends = Ends::of(&moving.order);
        let listed = match collected {
            Ok(listed) if !moving.cut => listed,
            Ok(_) => {
                let cut = format!("nothing was put at {}", ends.to);
                return self.transferred(io, id, Err(cut));
            }
            Err(error) => {
                let failed = format!("collecting {} failed: {error}", ends.from);
                return self.transferred(io, id, Err(failed));
            }
        };
        let bytes = (listed.iter())
            .map(|entry| match entry {
                Entry::Dir { .. } => 0,
                Entry::File { size, .. } => *size,
            })
            .sum();
        moving.moved = (listed.len() as u64, bytes);
        let sink = if ends.into_runner {
            *runner
        } else {
            moving.order.holder
        };
        let staging = lease.child.dir.join(STAGING);
        // Each entry with the mode its source sent, which the host's umask may have narrowed in
        // `staging`.
        let puts = (listed.into_iter())
            .map(|entry| match entry {
                Entry::Dir { path, mode } => Put::Dir(path, mode),
                Entry::File { path, mode, .. } => {
                    let host = staging.join(path.as_str());
                    Put::Read(path, mode, host)
                }
            })
            .collect();
        let put = Ask::Put(ends.to, ends.root, puts);
        self.lease_ask(io, (id, Step::Putting), sink, put);
    }

    // Ends the transfer, which moved its entries and their files' bytes or failed, once its
    // staging is removed.
    fn transferred(
        &mut self,
        io: &mut Io<'r, 's, H>,
        id: LeaseId,
        moved: Result<(u64, u64), String>,
    ) {
        let lease = self.lease(id);
        let Some(moving) = lease.transfer.take() else {
            unreachable!("a lease ends only the transfer under way")
        };
        lease.step = Step::Idle;
        let staging = lease.child.dir.join(STAGING);
        if let Err(error) = fs::remove_dir_all(&staging)
            && error.kind() != io::ErrorKind::NotFound
        {
            self.note(&format!("{} can't be removed: {error}", staging.display()));
        }
        let end = match moved {
            Ok((entries, bytes)) => TransferEnd::Moved { entries, bytes },
            Err(reason) if moving.cut => {
                TransferEnd::Failed(format!("the lease's end cut the transfer short: {reason}"))
            }
            Err(reason) => TransferEnd::Failed(reason),
        };
        let transfer = moving.order.number;
        let event = Event::TransferEnded {
            lease: id,
            transfer,
            end,
        };
        self.input(Input::Core(event));
        self.lease_next(io, id);
    }

    // Closes the lease once the core's stop or its runner's failure has come and no step is under
    // way but its command's run: freezes its runner, whose collects follow.
    fn lease_next(&mut self, io: &mut Io<'r, 's, H>, id: LeaseId) {
        let lease = self.lease(id);
        let settled = matches!(lease.step, Step::Idle | Step::Running);
        if !settled || !(lease.stopped || lease.failure.is_some()) {
            return;
        }
        let Some((_, session)) = lease.child.started.clone() else {
            unreachable!("a settled lease holds its runner")
        };
        self.lease_ask(io, (id, Step::Freezing), session, Ask::Freeze);
    }

    // Collects the lease's `part`: the output of the command that still ran, then `out`; then
    // deletes its runner.
    fn collect_lease(&mut self, io: &mut Io<'r, 's, H>, id: LeaseId, part: u8) {
        let lease = self.lease(id);
        let Some((_, session)) = lease.child.started.clone() else {
            unreachable!("a lease collects only from its runner")
        };
        let dir = match part {
            0 => lease
                .running
                .as_ref()
                .map(|running| command_dir(running.command)),
            1 => Some(out_dir()),
            _ => None,
        };
        match (part, dir) {
            (0, None) => self.collect_lease(io, id, 1),
            (_, Some(dir)) => {
                let ask = collect(&dir, &lease.child.dir.join(dir.as_str()));
                self.lease_ask(io, (id, Step::Closing(part)), session, ask);
            }
            (_, None) => {
                let event = match lease.failure.take() {
                    Some(reason) => Event::LeaseFailed { lease: id, reason },
                    None => Event::LeaseEnded { lease: id },
                };
                self.lease_delete(io, id, event);
            }
        }
    }

    // Deletes the lease's runner, whose delete `after` follows.
    fn lease_delete(&mut self, io: &mut Io<'r, 's, H>, id: LeaseId, after: Event) {
        let lease = self.lease(id);
        lease.step = Step::Deleting;
        let child = (lease.child.which, lease.child.started.take());
        self.close_child(io, child, End::Event(after));
    }
}

/// Where a transfer's tree goes, and the rule that a failed put restates.
struct Ends {
    from: AbsolutePath,
    to: AbsolutePath,
    root: Root,
    rule: &'static str,
    // Whether it goes from the holder into the runner.
    into_runner: bool,
}

impl Ends {
    fn of(order: &Transfer) -> Self {
        let new = ", which must not exist";
        let (from, to, root, rule, into_runner) = match &order.direction {
            Direction::Put { tree, to, replace } if !*replace => {
                let rule = ", which must not exist: give `--replace` to replace it";
                (tree, to, Root::New, rule, true)
            }
            Direction::Put { tree, to, .. } => (tree, to, Root::Replaced, "", true),
            Direction::Get { from, to } => (from, to, Root::New, new, false),
        };
        Self {
            from: from.clone(),
            to: to.clone(),
            root,
            rule,
            into_runner,
        }
    }
}

/// `run` in the runner's `cwd` with its `env`, its output in [`command_dir`].
fn launch(runner: &Runner, run: &Run) -> Launch {
    let dir = command_dir(run.command);
    let output = |name: RelativePath| pumpkin_dir().join(&dir.join(&name));
    Launch {
        argv: run.argv.clone(),
        env: command_env(&runner.env),
        cwd: runner.cwd.clone(),
        stdin: None,
        stdout: output(const { RelativePath::from_static("command.stdout") }),
        stderr: output(const { RelativePath::from_static("command.stderr") }),
    }
}

fn command_ended(lease: LeaseId, command: u64, ended: (CommandEnd, String, String)) -> Event {
    let (end, stdout, stderr) = ended;
    Event::CommandEnded {
        lease,
        command,
        end,
        stdout,
        stderr,
    }
}

/// `log/<command>`, the directory of the `command`th command's output of a lease's, or of a body's.
pub(crate) fn command_dir(command: u64) -> RelativePath {
    log_dir().join(&RelativePath::from(command))
}

/// The end of the output in the file at `path`, as [`op::output_tail`] gives it.
fn tail(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let most = OUTPUT_TAIL_BYTES as u64;
    file.seek(SeekFrom::Start(size.saturating_sub(most)))?;
    let mut bytes = Vec::with_capacity(OUTPUT_TAIL_BYTES);
    file.take(most).read_to_end(&mut bytes)?;
    Ok(op::output_tail(&bytes))
}
