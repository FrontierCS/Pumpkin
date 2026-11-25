//! Leases, each a child of the run ([`child`](super::child)): once its runner is ready, a lease
//! runs the commands and transfers the core orders ([`Order`]), one at a time, and the core is told
//! how each ended. A command that runs past its timeout, if it has one, is killed alone
//! ([`Session::kill`]). The core's stop closes the lease: its runner is frozen, and its `out`, and
//! the log of a command that still ran, are collected.
//!
//! A runner's sandbox holds a session for `main` and for each of its companions. The end of any
//! session fails the lease.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::iter;
use std::num::NonZeroU64;
use std::path::Path;
use std::time::Duration;

use decision::{Direction, Event, LeaseId};
use gateway::{Arrival, Process, Put, Session, Trees};
use schema::ir::{Argv, Container};
use schema::sandbox::op::{self, CommandEnd, OUTPUT_TAIL_BYTES, TransferEnd};
use schema::sandbox::tree::Limits;
use schema::sandbox::{
    Entry, Launch, RelativePath, Root, command_env, dir_mode, log_dir, out_dir, pumpkin_dir,
};
use schema::workflow::Runner;
use stdx::Context;
use stdx::time::Clock as _;

use super::child::{Child, End, Message};
use super::sandbox::{ENTRIES, Sessions, collect};
use super::{Clock, Host, Inbox};

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
    /// The runner's container that the tree goes to or comes from.
    pub(super) container: Container,
    pub(super) direction: Direction,
    /// The most bytes of files it may move.
    pub(super) max_bytes: NonZeroU64,
    /// The holder's sandbox's trees.
    pub(super) holder: Trees,
}

/// The core's order to run a command in a lease's runner, its `command`th.
pub(super) struct Run {
    pub(super) command: u64,
    /// The runner's container that it runs in.
    pub(super) container: Container,
    pub(super) argv: Argv,
    /// The longest it may run, if it has a bound of its own.
    pub(super) timeout: Option<Duration>,
    pub(super) required_output: bool,
}

struct Running {
    command: u64,
    /// The container that it runs in, whose session started it.
    container: Container,
    process: Process,
    /// When it is to be killed, since the run started, until it has been.
    kill_at: Option<Duration>,
    timed_out: bool,
    required_output: bool,
}

/// The session of `container` among `sessions`, a runner's, or why a lease serves nothing in it.
fn session_of<'s, C>(
    sessions: &'s mut Sessions<C>,
    container: &Container,
) -> Result<&'s mut Session<C>, String> {
    match container {
        Container::Main => Ok(&mut sessions.main),
        Container::Companion(name) => Err(format!(
            "the runner's companion `{name}` takes no command or copy: run commands and copy \
             trees in `{}`",
            Container::Main
        )),
    }
}

/// A lease, as its thread drives it.
#[derive(Clone, Copy)]
pub(super) struct Leased<'r> {
    pub(super) lease: LeaseId,
    pub(super) runner: &'r Runner,
    /// By when what the lease puts in its sandbox, each command's start and each kill are done,
    /// as a time since the run started.
    pub(super) start: Duration,
}

pub(super) fn run<H: Host>(leased: &Leased<'_>, child: &mut Child<'_, H, Order>) {
    super::child::run(child, None, |child, sessions| {
        End::Event(drive(leased, child, sessions))
    });
}

/// Runs the commands the core orders in `child`'s sandbox, whose sessions are `sessions`, until
/// the core's stop, and returns the report of the lease's end.
fn drive<H: Host>(
    leased: &Leased<'_>,
    child: &mut Child<'_, H, Order>,
    sessions: &mut Sessions<H::Clock>,
) -> Event {
    let lease = leased.lease;
    let failed = |reason: String| Event::LeaseFailed { lease, reason };
    // What reached the lease as its sandbox started. The core's stop ends it. Otherwise the
    // first end of a session fails it, as an end that comes once it is ready does.
    let mut early = None;
    while let Some(message) = child.inbox.take() {
        match message {
            Message::Stop => return Event::LeaseEnded { lease },
            Message::Session(container, _, Arrival::Ended { reason }) => {
                early.get_or_insert((container, reason));
            }
            // The core orders a lease nothing until it is ready, and none of its commands runs.
            Message::Session(
                _,
                _,
                Arrival::Exited { .. }
                | Arrival::Op(_)
                | Arrival::Refused(_)
                | Arrival::ModelRequest(_),
            )
            | Message::Order(_) => {}
        }
    }
    if let Some((container, reason)) = early {
        return failed(ended(&container, &reason));
    }
    if let Err(error) = std::fs::create_dir(child.dir.join(log_dir().as_str())) {
        return failed(format!(
            "the lease's log can't be made in the record: {error}"
        ));
    }
    child.event(Event::LeaseReady { lease });
    let mut running = None;
    let served = serve(leased, child, sessions, &mut running);
    let by = child.deadlines.collect;
    if let Err(reason) = freeze(sessions, running.as_ref(), by) {
        return failed(served.err().unwrap_or(reason));
    }
    if let Some(running) = running {
        let dir = command_dir(running.command);
        let session = session_of(sessions, &running.container);
        let collected = session.and_then(|session| collect(session, &dir, &child.dir, by));
        if let Err(error) = collected {
            child.note(format!("lease {}'s {dir} is incomplete: {error}", lease.0));
        }
    }
    if let Err(error) = collect(&mut sessions.main, &out_dir(), &child.dir, by) {
        child.note(format!("lease {}'s out is incomplete: {error}", lease.0));
    }
    served.map_or_else(failed, |()| Event::LeaseEnded { lease })
}

/// Freezes, by `by`, the containers whose files the record keeps at a lease's end. They are `main`,
/// whose `out` it collects, and that of `running`, the command that still runs, if one does.
fn freeze<C: Clock>(
    sessions: &mut Sessions<C>,
    running: Option<&Running>,
    by: Duration,
) -> Result<(), String> {
    let still = running.map(|running| &running.container);
    let elsewhere = still.filter(|container| **container != Container::Main);
    for container in iter::once(&Container::Main).chain(elsewhere) {
        let frozen = session_of(sessions, container)?.freeze(by);
        frozen.map_err(|error| format!("freezing the runner's `{container}` failed: {error}"))?;
    }
    Ok(())
}

/// Runs the commands and transfers the core orders in `child`'s sandbox, whose sessions are
/// `sessions`, one at a time, until the core's stop, which leaves in `running` the command that
/// still runs then, if one does; or fails with why the runner failed first.
#[expect(clippy::too_many_lines, reason = "one arm per kind of message")]
fn serve<H: Host>(
    leased: &Leased<'_>,
    child: &mut Child<'_, H, Order>,
    sessions: &mut Sessions<H::Clock>,
    running: &mut Option<Running>,
) -> Result<(), String> {
    let Leased {
        lease,
        runner,
        start: by_start,
    } = *leased;
    // What a transfer took from the inbox as it cut it short, to act on next.
    let mut held = None;
    loop {
        let kill_at = running.as_ref().and_then(|running| running.kill_at);
        let message = held
            .take()
            .or_else(|| child.inbox.wait(kill_at.unwrap_or(Duration::MAX)));
        match message {
            None => {
                if let Some(running) = running {
                    kill(child, by_start, running, sessions)?;
                }
            }
            Some(Message::Stop) => return Ok(()),
            Some(Message::Order(Order::Transfer(order))) => {
                let end = transfer(child, sessions, &order, &mut held);
                let transfer = order.number;
                child.event(Event::TransferEnded {
                    lease,
                    transfer,
                    end,
                });
            }
            Some(Message::Order(Order::Run(run))) => {
                match start(runner, (&run, by_start), sessions) {
                    Ok(process) => {
                        let kill_at = run.timeout.map(|timeout| child.clock.now() + timeout);
                        *running = Some(Running {
                            command: run.command,
                            container: run.container,
                            process,
                            kill_at,
                            timed_out: false,
                            required_output: run.required_output,
                        });
                    }
                    Err(error) => {
                        let ended = (CommandEnd::Unstarted, String::new(), error);
                        child.event(command_ended(lease, run.command, ended));
                    }
                }
            }
            Some(Message::Session(container, _, Arrival::Exited { process, status })) => {
                // A process of one container may have the number of another's.
                let ran = running.take_if(|running| {
                    running.container == container && running.process == process
                });
                let Some(ran) = ran else {
                    continue;
                };
                let end = if ran.timed_out {
                    CommandEnd::TimedOut
                } else {
                    CommandEnd::Exited(status)
                };
                let session = session_of(sessions, &ran.container)?;
                let (stdout, stderr) = outputs(child, ran.command, session, ran.required_output)?;
                child.event(command_ended(lease, ran.command, (end, stdout, stderr)));
            }
            Some(Message::Session(container, _, Arrival::Ended { reason })) => {
                return Err(ended(&container, &reason));
            }
            // A type for a passive session's arrivals would split the gateway's `pub` `Arrival` by
            // the kind of session.
            Some(Message::Session(
                _,
                _,
                Arrival::Op(_) | Arrival::Refused(_) | Arrival::ModelRequest(_),
            )) => unreachable!(
                "one of lease {}'s passive sessions handed out an op or a model request, though \
                 each was accepted without an op payload limit and without model requests, so \
                 either ends the session instead, and arrives as its end",
                lease.0
            ),
        }
    }
}

/// Why a lease fails once the session of `container` of its runner has ended, for `reason`.
fn ended(container: &Container, reason: &str) -> String {
    match container {
        Container::Main => format!("the runner's session ended: {reason}"),
        Container::Companion(name) => {
            format!("the session of the runner's companion `{name}` ended: {reason}")
        }
    }
}

/// Kills `running`, whose timeout has passed, in its container, whose session `sessions` gives. A
/// command that isn't killed runs on until it exits, or the lease ends.
fn kill<H: Host>(
    child: &Child<'_, H, Order>,
    by_start: Duration,
    running: &mut Running,
    sessions: &mut Sessions<H::Clock>,
) -> Result<(), String> {
    running.kill_at = None;
    let session = session_of(sessions, &running.container)?;
    match session.kill(running.process, by_start) {
        Ok(()) => running.timed_out = true,
        Err(error) => {
            let reason = format!("command {} wasn't killed: {error}", running.command);
            if running.required_output {
                return Err(reason);
            }
            child.note(reason);
        }
    }
    Ok(())
}

/// Starts `run` in its container, whose session `sessions` gives, in the runner's `cwd` with its
/// `env`, which are `main`'s, the one container with a session. Its output goes in
/// [`command_dir`].
fn start(
    runner: &Runner,
    (run, by): (&Run, Duration),
    sessions: &mut Sessions<impl Clock>,
) -> Result<Process, String> {
    let session = session_of(sessions, &run.container)?;
    let dir = command_dir(run.command);
    let put = session.put(&pumpkin_dir(), [Put::Dir(dir.clone(), dir_mode())], by);
    put.context(format_args!("making {dir} failed"))?;
    let output = |name: RelativePath| pumpkin_dir().join(&dir.join(&name));
    let launch = Launch {
        argv: run.argv.clone(),
        env: command_env(&runner.env),
        cwd: runner.cwd.clone(),
        stdin: None,
        stdout: output(const { RelativePath::from_static("command.stdout") }),
        stderr: output(const { RelativePath::from_static("command.stderr") }),
    };
    (session.start(launch, by)).context("the command didn't start")
}

/// Collects the output of the lease's `command`th command into the lease's record, and returns
/// the ends of its standard output and error.
fn outputs<H: Host>(
    child: &Child<'_, H, Order>,
    command: u64,
    session: &mut Session<H::Clock>,
    required: bool,
) -> Result<(String, String), String> {
    let (dir, by) = (command_dir(command), child.deadlines.collect);
    if let Err(error) = collect(session, &dir, &child.dir, by) {
        let reason = format!("command {command}'s output is incomplete: {error}");
        if required {
            return Err(reason);
        }
        child.note(reason);
    }
    let collected = child.dir.join(dir.as_str());
    let tail = |name: &str| {
        let path = collected.join(name);
        match tail(&path) {
            Ok(text) => Ok(text),
            Err(error) if !required && error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => {
                let reason = format!("{} can't be read: {error}", path.display());
                if required {
                    return Err(reason);
                }
                child.note(reason);
                Ok(String::new())
            }
        }
    };
    Ok((tail("command.stdout")?, tail("command.stderr")?))
}

/// Copies the tree that `order` names between its holder's sandbox and its container of the
/// runner's, whose session `sessions` gives, through [`STAGING`], which it removes, and returns
/// how it ended. It is cut short once anything reaches the lease's inbox, which it keeps in `held`.
fn transfer<H: Host>(
    child: &mut Child<'_, H, Order>,
    sessions: &mut Sessions<H::Clock>,
    order: &Transfer,
    held: &mut Option<Message<Order>>,
) -> TransferEnd {
    let runner = match session_of(sessions, &order.container) {
        Ok(session) => session.trees(),
        Err(reason) => return TransferEnd::Failed(reason),
    };
    let staging = child.dir.join(STAGING);
    let mut stopped = || {
        if held.is_none() {
            *held = child.inbox.take();
        }
        held.is_some()
    };
    let by = (&child.clock, child.deadlines.collect);
    let moved = staged_move(&runner, order, &staging, by, &mut stopped);
    // The core's stop; the runner's session's end says itself why the transfer failed.
    let cut = matches!(held, Some(Message::Stop));
    if let Err(error) = fs::remove_dir_all(&staging)
        && error.kind() != io::ErrorKind::NotFound
    {
        child.note(format!("{} can't be removed: {error}", staging.display()));
    }
    match moved {
        Ok((entries, bytes)) => TransferEnd::Moved { entries, bytes },
        Err(reason) if cut => {
            TransferEnd::Failed(format!("the lease's end cut the transfer short: {reason}"))
        }
        Err(reason) => TransferEnd::Failed(reason),
    }
}

/// Copies the tree that `order` names into `staging`, then out of it, by `by`, as [`transfer`]
/// does, and returns its entries and its files' bytes.
fn staged_move(
    runner: &Trees,
    order: &Transfer,
    staging: &Path,
    by: (&impl Clock, Duration),
    stopped: &mut dyn FnMut() -> bool,
) -> Result<(u64, u64), String> {
    // Where the tree goes, and the rule that a failed put restates.
    let (source, from, sink, to, root, rule) = match &order.direction {
        Direction::Put {
            tree,
            to,
            replace: false,
        } => {
            let rule = ", which must not exist: give `--replace` to replace it";
            (&order.holder, tree, runner, to, Root::New, rule)
        }
        Direction::Put {
            tree,
            to,
            replace: true,
        } => (&order.holder, tree, runner, to, Root::Replaced, ""),
        Direction::Get { from, to } => {
            let rule = ", which must not exist";
            (runner, from, &order.holder, to, Root::New, rule)
        }
    };
    let to_where = match &order.direction {
        Direction::Put { .. } => "in the runner",
        Direction::Get { .. } => "here",
    };
    let limits = Limits {
        bytes: order.max_bytes.get(),
        entries: ENTRIES,
    };
    let collected = source.collect(from, staging, limits, by, stopped);
    let listed = collected.context(format_args!("collecting {from} failed"))?;
    let entries = listed.len() as u64;
    let bytes = (listed.iter())
        .map(|entry| match entry {
            Entry::Dir { .. } => 0,
            Entry::File { size, .. } => *size,
        })
        .sum();
    // Why the put went short, if it did: a file that couldn't be opened, or the lease's end.
    let mut cut = None;
    let mut listed = listed.into_iter();
    let puts = iter::from_fn(|| {
        if stopped() {
            cut = Some(format!("{to} holds only part of the tree"));
            return None;
        }
        // Each entry with the mode its source sent, which the host's umask may have narrowed in
        // `staging`.
        match listed.next()? {
            Entry::Dir { path, mode } => Some(Put::Dir(path, mode)),
            Entry::File { path, mode, .. } => match File::open(staging.join(path.as_str())) {
                Ok(file) => Some(Put::Read(path, mode, file)),
                Err(error) => {
                    cut = Some(format!("{path} can't be read back on the host: {error}"));
                    None
                }
            },
        }
    });
    let put = sink.put(to, root, puts, by).map_err(|error| {
        format!(
            "putting the tree at {to} {to_where} failed: {error}: the tree's root becomes \
             {to}{rule}"
        )
    });
    match cut {
        Some(cut) => Err(cut),
        None => put.map(|()| (entries, bytes)),
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
