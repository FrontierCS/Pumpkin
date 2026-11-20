//! Bodies, as the runtime calls them: a body is a function of the workflow's, which the runtime
//! reaches through the driver's [`Bodies`], and so depends on no language. A job's body drives a
//! sandbox, its target, through a [`Target`]. Each
//! command that a body runs has its output in a directory of its own, `log/<c>` for its `c`th, as
//! a lease's command ([`command_dir`]), collected into the job's directory of the record once it
//! exits, or at the call's end if the call is cut short while it runs. Fresh targets are frozen
//! and collected as the body ends. A work-lifetime sandbox stays available until work ends.

use core::num::NonZeroU64;
use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::PathBuf;
use std::time::Duration;
use std::{fs, io};

use decision::{Event, JobId, PayloadRef};
use gateway::{Process, Put, Session};
use schema::ir::{self, AbsolutePath, Argv, Name, TargetLifetime};
use schema::sandbox::op::{
    CommandEnd, CommandStatus, Last, LeaseStatusReply, RunnerOutput, SessionOp, TransferEnd,
    TransferStatus,
};
use schema::sandbox::tree::Limits;
use schema::sandbox::{
    ExitStatus, Launch, RESULT_MAX_BYTES, RelativePath, Root, command_env, dir_mode, file_mode,
    log_dir, out_dir, pumpkin_dir,
};
use schema::workflow::{self, Final, JobFunction, TargetId};
use serde_json::{Map, Value};
use stdx::{Context, Deadline};

use crate::child::{Child, Report};
use crate::host::deadline;
use crate::lease::command_dir;
use crate::record::{Stored, reference};
use crate::relay::{Relay, Waited};
use crate::sandbox::{collect, collect_as};
use crate::scoped::Waiting;
use crate::{Clock, Entry, Host, Targeted};

/// The evaluator of the workflow's bodies, which the driver gives the run. The run calls it on
/// the threads of its children, so it is shared between them.
pub trait Bodies: Sync {
    /// The stack that a thread calling a body needs.
    fn stack_bytes(&self) -> usize;

    /// Calls the job's body `body`, given `keywords` and its caller's `arguments`, with a handle on
    /// `target` and the payload, on the calling thread, whose stack is
    /// [`stack_bytes`](Self::stack_bytes).
    ///
    /// # Errors
    ///
    /// Why the body didn't return: what its evaluation refused or failed on, a method of
    /// `target`'s that failed included.
    fn job(
        &self,
        body: &ir::Body,
        keywords: &Map<String, Value>,
        arguments: &BTreeMap<Name, ir::ArgumentValue>,
        target: &mut dyn Target,
    ) -> Result<Vec<(Name, Value)>, String>;

    /// Calls the final body with one entry per agent, in role order. Each submission names
    /// an input slot for [`Target::put`] or [`Target::capture`] on this call.
    ///
    /// # Errors
    ///
    /// As [`job`](Self::job)'s.
    fn final_call(
        &self,
        body: &ir::FinalBody,
        keywords: &Map<String, Value>,
        submissions: &[(Name, Option<usize>)],
        target: &mut dyn Target,
    ) -> Result<Vec<(Name, Value)>, String>;

    /// Calls the now function's body `body`, given its caller's `arguments`, with a handle on
    /// `run` and the payload, on the calling thread, whose stack is
    /// [`stack_bytes`](Self::stack_bytes).
    ///
    /// # Errors
    ///
    /// Why the body didn't return: what its evaluation refused or failed on.
    fn now(
        &self,
        body: &ir::Body,
        arguments: &BTreeMap<Name, ir::ArgumentValue>,
        run: &mut dyn RunState,
    ) -> Result<Vec<(Name, Value)>, String>;
}

/// The run's state, as a now function's body proposes effects on it, which the core applies.
pub trait RunState {
    /// Proposes that the call's payload become the caller's submission, replacing the last.
    fn set_submission(&mut self);

    /// Proposes locking the caller's submission and ending its work.
    fn end_work(&mut self);
}

/// The target of a job's call, as its body drives it. A method's `Err` is why it failed, which
/// ends the call.
pub trait Target {
    /// Waits for a fresh runner to be ready and returns a lease number unique within this call.
    fn runner_open(&mut self, grant: &Name) -> Result<u64, String>;

    /// Copies the target's directory to a new directory on its runner.
    fn runner_put(
        &mut self,
        grant: &Name,
        lease: u64,
        tree: AbsolutePath,
        to: AbsolutePath,
    ) -> Result<(), String>;

    /// Waits for a started command's exit or timeout and both recorded output streams.
    fn runner_run(
        &mut self,
        grant: &Name,
        lease: u64,
        argv: Argv,
        timeout_seconds: Option<NonZeroU64>,
    ) -> Result<RunnerOutput, String>;

    /// Waits for deletion of the callback's runner.
    fn runner_close(&mut self, grant: &Name, lease: u64) -> Result<(), String>;

    /// The issuing agent's declared role name, or none in the final body.
    fn caller(&self) -> Option<&str>;

    /// Puts the input at `path`, one file, its parents made. A job's payload has slot zero.
    fn put(&mut self, path: &AbsolutePath, input_slot: usize) -> Result<(), String>;

    /// Runs `argv`, and waits for it to end, whatever its exit status.
    fn run(&mut self, argv: &Argv) -> Result<(), String>;

    /// Runs `argv` with the optional input slot as stdin. Returns its complete UTF-8 stdout,
    /// at most [`RESULT_MAX_BYTES`], on exit code zero.
    fn capture(&mut self, argv: &Argv, stdin: Option<usize>) -> Result<String, String>;

    /// The text of the file at `path`.
    fn get(&mut self, path: &AbsolutePath) -> Result<String, String>;
}

/// What a body gets from its target: one file, of at most [`RESULT_MAX_BYTES`].
const GOT: Limits = Limits {
    bytes: RESULT_MAX_BYTES,
    entries: 1,
};

/// A call of a body on a target that its child has started.
pub(crate) struct Call<'c> {
    pub(crate) job: JobId,
    /// The target, whose `cwd` and `env` its commands run with.
    pub(crate) target: &'c workflow::Target,
    pub(crate) of: &'c Of<'c>,
    pub(crate) inputs: Vec<Stored>,
    /// The longest the body may drive the target, from its start.
    pub(crate) timeout: Duration,
}

/// Whose body a call calls.
pub(crate) enum Of<'r> {
    /// A job function's, with the arguments its caller gave.
    Function {
        function: &'r JobFunction,
        arguments: BTreeMap<Name, ir::ArgumentValue>,
    },
    Final {
        final_call: &'r Final,
        submissions: Vec<(Name, Option<usize>)>,
    },
}

impl Of<'_> {
    pub(crate) fn target(&self) -> TargetId {
        match self {
            Self::Function { function, .. } => function.target,
            Self::Final { final_call, .. } => final_call.target,
        }
    }
}

/// How a body's call ended.
pub(crate) enum Ended {
    /// The body returned these names and values, in order.
    Returned(Vec<(Name, Value)>),
    /// The body didn't return, for this reason.
    Failed(String),
    /// The call's timeout passed while the body drove the target.
    TimedOut,
    /// The core stopped the call.
    Stopped,
}

/// Calls `call`'s body through `bodies`, with a handle on the target that `session` reaches,
/// on the thread of `child`, and reports the body's start. Everything the body asks of the target
/// is to be done by the call's timeout, from the body's start; a stop of the core's, and the
/// timeout's passing, cut the call short at the body's next ask of the target, or while a command
/// it ran runs. Meanwhile relays the target's ops and model requests to the loop.
pub(crate) fn evaluate<H: Host>(
    bodies: &dyn Bodies,
    call: &Call<'_>,
    child: &mut Child<'_, H, crate::relay::Order>,
    session: &mut Session,
    stdin: &mut Stdin,
) -> Ended {
    let resident = call.target.lifetime == TargetLifetime::Work;
    if !resident && child.stopped() {
        return Ended::Stopped;
    }
    // The timeout runs from the start's reported time.
    let at = child.clock.now();
    child.report(Report::Event(at, Event::CallStarted { job: call.job }));
    let mut target = OnTarget {
        call,
        child,
        session,
        timeout: at.saturating_add(call.timeout),
        relay: Relay::default(),
        cut: None,
        running: None,
        stdin,
        runs_count: 0,
        gets_count: 0,
    };
    let ready = if resident {
        target.going().map(drop)
    } else {
        Ok(())
    };
    let returned = ready.and_then(|()| match call.of {
        Of::Function {
            function,
            arguments,
        } => bodies.job(&function.body, &function.keywords, arguments, &mut target),
        Of::Final {
            final_call,
            submissions,
        } => bodies.final_call(
            &final_call.body,
            &final_call.keywords,
            submissions,
            &mut target,
        ),
    });
    let returned = if resident && target.cut.is_none() {
        target.going().and(returned)
    } else {
        returned
    };
    if resident {
        if let Err(reason) = target.release() {
            child.event(Event::SharedFailed {
                target: call.of.target(),
                reason: reason.clone(),
            });
            return Ended::Failed(reason);
        }
    } else {
        target.collect_fresh();
    }
    match (target.cut, returned) {
        (Some(Cut::TimedOut), _) => Ended::TimedOut,
        (Some(Cut::Stopped), _) => Ended::Stopped,
        (None, Ok(values)) => Ended::Returned(values),
        (None, Err(reason)) => Ended::Failed(reason),
    }
}

/// What cut a call short while its body drove the target.
#[derive(Clone, Copy)]
enum Cut {
    TimedOut,
    Stopped,
}

impl Cut {
    /// Why each of the target's methods fails once the call is cut short.
    fn reason(self) -> String {
        match self {
            Self::TimedOut => "the call's timeout passed".to_owned(),
            Self::Stopped => "the call was stopped".to_owned(),
        }
    }
}

/// The input directory on one sandbox, reused only by its serialized calls.
#[derive(Default)]
pub(crate) enum Stdin {
    #[default]
    Unused,
    Empty,
    Staged,
}

impl Stdin {
    const DIR: AbsolutePath = AbsolutePath::from_static("/pumpkin/.stdin");

    fn put(
        &mut self,
        session: &Session,
        bytes: &[u8],
        by: Deadline,
    ) -> Result<AbsolutePath, String> {
        let trees = session.trees();
        let dir = Self::DIR;
        if matches!(self, Self::Unused) {
            trees
                .put(&dir, Root::New, [], by)
                .context("reserving the command's stdin directory failed")?;
            *self = Self::Empty;
        }
        let root = if matches!(self, Self::Empty) {
            Root::Kept
        } else {
            Root::Replaced
        };
        let name = const { RelativePath::from_static("input") };
        let path = dir.join(&name);
        // Even a failed put can leave a partial file.
        *self = Self::Staged;
        trees
            .put(&dir, root, [Put::File(name, file_mode(), bytes)], by)
            .context("staging the command's stdin failed")?;
        Ok(path)
    }

    fn clear(&mut self, session: &Session, by: Deadline) -> Result<(), String> {
        if matches!(self, Self::Staged) {
            session
                .trees()
                .put(&Self::DIR, Root::Replaced, [], by)
                .context("clearing the shared call's stdin failed")?;
            *self = Self::Empty;
        }
        Ok(())
    }
}

/// The target of a body's call, on its child's session.
struct OnTarget<'a, 'c, 'r, H: Host> {
    call: &'a Call<'c>,
    child: &'a mut Child<'r, H, crate::relay::Order>,
    session: &'a mut Session,
    /// When the call's timeout passes, as a time since the run started.
    timeout: Duration,
    relay: Relay,
    /// What cut the call short, after which every method fails.
    cut: Option<Cut>,
    running: Option<(u64, Process)>,
    stdin: &'a mut Stdin,
    /// The commands run so far.
    runs_count: u64,
    /// The files got so far, which number their directories in the record.
    gets_count: u64,
}

impl<H: Host> OnTarget<'_, '_, '_, H> {
    /// The deadline of the body's next ask of the target, if the call goes on: relays what the
    /// target's session handed over since the last, and fails once the core has stopped the call,
    /// or its timeout has passed.
    fn going(&mut self) -> Result<Deadline, String> {
        if let Some(cut) = self.cut {
            return Err(cut.reason());
        }
        let by = deadline(&self.child.clock, self.timeout);
        let idle = (self.relay).idle(self.child, self.call.job, (self.session, by));
        match idle {
            Ok(true) => return Err(self.cut_short(Cut::Stopped)),
            Ok(false) => {}
            Err(reason) => return Err(format!("the target's session ended: {reason}")),
        }
        if self.child.clock.now() >= self.timeout {
            return Err(self.cut_short(Cut::TimedOut));
        }
        Ok(by)
    }

    fn scoped(
        &mut self,
        grant: &Name,
        waiting: Waiting,
        op: SessionOp,
    ) -> Result<LeaseStatusReply, String> {
        self.going()?;
        let waited = self.relay.runner(
            self.child,
            self.call.job,
            self.timeout,
            self.session,
            grant,
            (waiting, op),
        );
        match self.timely(waited)? {
            // A queued receipt can wake the body after its deadline, including the final close.
            Waited::Finished(status) => self.going().map(|_| status),
            Waited::Stopped => Err(self.cut_short(Cut::Stopped)),
            Waited::TimedOut => Err(self.cut_short(Cut::TimedOut)),
        }
    }

    /// Reports the timeout, if any, and returns why the method fails.
    fn cut_short(&mut self, cut: Cut) -> String {
        if let Cut::TimedOut = cut {
            self.child.event(Event::CallTimedOut { job: self.call.job });
        }
        self.cut = Some(cut);
        cut.reason()
    }

    /// `done`, what came of an ask of the target, whose failure once the call's timeout, its
    /// deadline, has passed is the timeout's.
    fn timely<T>(&mut self, done: Result<T, String>) -> Result<T, String> {
        match done {
            Err(_) if self.child.clock.now() >= self.timeout => Err(self.cut_short(Cut::TimedOut)),
            done => done,
        }
    }

    /// Logs what the body asked of the target, now.
    fn log(&self, targeted: Targeted) {
        let at = self.child.clock.now();
        self.child.log(Entry::Target(self.call.job, targeted, at));
    }

    /// The recorded output directory, only once the whole command log has arrived.
    fn collect_output(&mut self, command: u64, by: Deadline) -> Result<PathBuf, String> {
        let dir = self.output_dir(command);
        let logs = self.child.dir.join(log_dir().as_str());
        let into = self.child.dir.join(command_dir(command).as_str());
        let collected = fs::create_dir_all(&logs)
            .context(logs.display())
            .and_then(|()| collect_as(self.session, &dir, &into, by));
        if let Err(error) = &collected {
            let job = self.call.job.0;
            self.child
                .note(format!("job {job}'s {dir} is incomplete: {error}"));
        }
        collected.map(|()| into)
    }

    /// Stops and accounts for the active client before releasing this shared sandbox to another
    /// job. An unconfirmed stop or closed session makes the target unavailable.
    fn release(&mut self) -> Result<(), String> {
        let until = self
            .child
            .clock
            .now()
            .saturating_add(self.call.timeout)
            .min(self.child.deadlines.collect);
        let by = deadline(&self.child.clock, until);
        if let Some((command, process)) = self.running {
            let stopped = self.stop_client(process, until, by);
            let _ = self.collect_output(command, by);
            let status = stopped?;
            let end = if matches!(self.cut, Some(Cut::TimedOut)) {
                CommandEnd::TimedOut
            } else {
                CommandEnd::Exited(status)
            };
            self.log(Targeted::Ended { command, end });
        }
        self.stdin.clear(self.session, by)?;
        match self.session.ended() {
            Some(reason) => Err(format!("the shared role's session ended: {reason}")),
            None => Ok(()),
        }
    }

    fn stop_client(
        &mut self,
        process: Process,
        until: Duration,
        by: Deadline,
    ) -> Result<ExitStatus, String> {
        self.session
            .kill(process, by)
            .context("stopping the shared call's command failed")?;
        loop {
            match self
                .relay
                .wait(self.child, self.call.job, until, process, self.session)?
            {
                Waited::Finished(status) => return Ok(status),
                Waited::Stopped => {}
                Waited::TimedOut => {
                    return Err("the shared call's command did not confirm its exit".to_owned());
                }
            }
        }
    }

    fn collect_fresh(&mut self) {
        let job = self.call.job.0;
        let by = deadline(&self.child.clock, self.child.deadlines.collect);
        match self.session.freeze(by) {
            Ok(()) => {}
            // Every ask of an ended session fails at once, with the reason it ended: collecting
            // would only repeat it.
            Err(error) if error.kind() == io::ErrorKind::ConnectionAborted => return,
            Err(error) => {
                (self.child).note(format!("freezing job {job}'s target failed: {error}"));
            }
        }
        if self.cut.is_some()
            && let Some((command, _)) = self.running
        {
            let _ = self.collect_output(command, by);
        }
        if let Err(error) = collect(self.session, &out_dir(), &self.child.dir, by) {
            (self.child).note(format!("job {job}'s out is incomplete: {error}"));
        }
    }

    fn input(&self, slot: usize) -> Result<&Stored, String> {
        self.call
            .inputs
            .get(slot)
            .ok_or_else(|| format!("this call has no input in slot {slot}"))
    }

    fn stage_stdin(
        &mut self,
        slot: usize,
        by: Deadline,
    ) -> Result<(AbsolutePath, PayloadRef), String> {
        let input = self.input(slot)?;
        let bytes = input.read()?;
        let supplied = input.reference.clone();
        let put = self.stdin.put(self.session, &bytes, by);
        Ok((self.timely(put)?, supplied))
    }

    fn command(&mut self, argv: &Argv, input: Option<usize>) -> Result<(u64, ExitStatus), String> {
        let by = self.going()?;
        let target = self.call.target;
        let command = self.runs_count;
        let dir = self.output_dir(command);
        self.runs_count += 1;
        let put = if target.lifetime == TargetLifetime::Work {
            self.session.put(&pumpkin_dir().join(&dir), [], by)
        } else {
            self.session
                .put(&pumpkin_dir(), [Put::Dir(dir.clone(), dir_mode())], by)
        };
        self.timely(put.context(format_args!("making {dir} failed")))?;
        let output = |name: RelativePath| pumpkin_dir().join(&dir.join(&name));
        let (stdin, supplied) = input
            .map(|slot| self.stage_stdin(slot, by))
            .transpose()?
            .unzip();
        let launch = Launch {
            argv: argv.clone(),
            env: command_env(&target.env),
            cwd: target.cwd.clone(),
            stdin,
            stdout: output(const { RelativePath::from_static("command.stdout") }),
            stderr: output(const { RelativePath::from_static("command.stderr") }),
        };
        let started = self.session.start(launch, by);
        let process = self.timely(started.context("the command didn't start"))?;
        self.running = Some((command, process));
        let argv = argv.clone();
        self.log(Targeted::Run {
            command,
            argv,
            stdin: supplied,
        });
        let job = self.call.job;
        let waited = (self.relay).wait(self.child, job, self.timeout, process, self.session);
        match waited {
            Ok(Waited::Finished(status)) => {
                self.running = None;
                let end = CommandEnd::Exited(status);
                self.log(Targeted::Ended { command, end });
                Ok((command, status))
            }
            Ok(Waited::TimedOut) => Err(self.cut_short(Cut::TimedOut)),
            Ok(Waited::Stopped) => Err(self.cut_short(Cut::Stopped)),
            Err(reason) => Err(format!("the target's session ended: {reason}")),
        }
    }

    fn output_dir(&self, command: u64) -> RelativePath {
        let dir = command_dir(command);
        if self.call.target.lifetime == TargetLifetime::Work {
            const { RelativePath::from_static("jobs") }
                .join(&RelativePath::from(self.call.job.0))
                .join(&dir)
        } else {
            dir
        }
    }
}

impl<H: Host> Target for OnTarget<'_, '_, '_, H> {
    fn runner_open(&mut self, grant: &Name) -> Result<u64, String> {
        let op = SessionOp::Open {
            instance_seconds: None,
        };
        self.scoped(grant, Waiting::Open, op)
            .map(|ready| ready.lease)
    }

    fn runner_put(
        &mut self,
        grant: &Name,
        lease: u64,
        tree: AbsolutePath,
        to: AbsolutePath,
    ) -> Result<(), String> {
        let op = SessionOp::Put {
            lease,
            tree,
            to,
            replace: false,
        };
        let status = self.scoped(grant, Waiting::Work, op)?;
        let Some(Last::Transfer(TransferStatus { end: Some(end), .. })) = status.last else {
            return Err("the runner's copy has no receipt".to_owned());
        };
        match end {
            TransferEnd::Moved { .. } => Ok(()),
            TransferEnd::Failed(reason) => Err(reason),
        }
    }

    fn runner_run(
        &mut self,
        grant: &Name,
        lease: u64,
        argv: Argv,
        timeout_seconds: Option<NonZeroU64>,
    ) -> Result<RunnerOutput, String> {
        let op = SessionOp::Run {
            lease,
            argv,
            timeout_seconds,
        };
        let status = self.scoped(grant, Waiting::Work, op)?;
        let Some(Last::Command(CommandStatus {
            end: Some(end),
            stdout,
            stderr,
            ..
        })) = status.last
        else {
            return Err("the runner's command has no receipt".to_owned());
        };
        match end {
            CommandEnd::Unstarted => Err(format!("the command did not start: {stderr}")),
            CommandEnd::Exited(_) | CommandEnd::TimedOut => Ok(RunnerOutput {
                end,
                stdout_tail: stdout,
                stderr_tail: stderr,
            }),
        }
    }

    fn runner_close(&mut self, grant: &Name, lease: u64) -> Result<(), String> {
        self.scoped(grant, Waiting::Close, SessionOp::Close { lease })
            .map(drop)
    }

    fn put(&mut self, path: &AbsolutePath, input_slot: usize) -> Result<(), String> {
        let by = self.going()?;
        let Some((dir, name)) = path.split_last() else {
            return Err("`/` is the sandbox's root: name a file's path".to_owned());
        };
        let payload = self.input(input_slot)?.read()?;
        let put = self
            .session
            .put(&dir, [Put::File(name, file_mode(), &payload)], by);
        let put = put.context(format_args!("putting the payload at {path} failed"));
        self.timely(put)?;
        let put = reference(&payload);
        self.log(Targeted::Put {
            path: path.clone(),
            put,
        });
        Ok(())
    }

    fn caller(&self) -> Option<&str> {
        match self.call.of {
            Of::Function { function, .. } => Some(
                self.child
                    .sandboxes
                    .workflow()
                    .sandbox(function.holder.role())
                    .name
                    .as_str(),
            ),
            Of::Final { .. } => None,
        }
    }

    fn run(&mut self, argv: &Argv) -> Result<(), String> {
        let (command, _) = self.command(argv, None)?;
        let _ = self.collect_output(command, deadline(&self.child.clock, self.timeout));
        Ok(())
    }

    fn capture(&mut self, argv: &Argv, stdin: Option<usize>) -> Result<String, String> {
        let (command, status) = self.command(argv, stdin)?;
        let log = self.collect_output(command, deadline(&self.child.clock, self.timeout));
        match status {
            ExitStatus::Code(0) => {}
            ExitStatus::Code(code) => {
                return Err(format!("command {command} exited with code {code}"));
            }
            ExitStatus::Signal(signal) => {
                return Err(format!("command {command} ended by signal {signal}"));
            }
        }
        let stdout = self.timely(log)?.join("command.stdout");
        let file = fs::File::open(&stdout).context(stdout.display())?;
        let mut bytes = Vec::new();
        file.take(RESULT_MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .context(format_args!("reading command {command}'s stdout failed"))?;
        if bytes.len() as u64 > RESULT_MAX_BYTES {
            return Err(format!(
                "command {command}'s stdout exceeds the capture limit of {RESULT_MAX_BYTES} bytes"
            ));
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| format!("command {command}'s stdout is not UTF-8 text"))?;
        self.going()?;
        Ok(text)
    }

    fn get(&mut self, path: &AbsolutePath) -> Result<String, String> {
        let by = self.going()?;
        let Some((_, name)) = path.split_last() else {
            return Err("`/` is the sandbox's root: name a file's path".to_owned());
        };
        self.gets_count += 1;
        let gets = self.child.dir.join("got");
        fs::create_dir_all(&gets).context(gets.display())?;
        let into = gets.join(self.gets_count.to_string());
        let collected = self.session.collect(path, &into, GOT, by);
        self.timely(collected.context(format_args!("getting {path} failed")))?;
        let got = into.join(name.as_str());
        let bytes = fs::read(&got).context(format_args!("getting {path} failed"))?;
        let got = reference(&bytes);
        self.log(Targeted::Get {
            path: path.clone(),
            got,
        });
        String::from_utf8(bytes).map_err(|_| format!("{path} is not UTF-8 text: get only text"))
    }
}
