//! Bodies, as the runtime calls them: a body is a function of the workflow's, which the runtime
//! reaches through the driver's [`Bodies`], and so depends on no language. A job's body drives a
//! sandbox that the runtime has started for its call, its target, through a [`Target`]. Each
//! command that a body runs has its output in a directory of its own, `log/<c>` for its `c`th, as
//! a lease's command ([`command_dir`]), collected into the job's directory of the record once it
//! exits, or at the call's end if the call is cut short while it runs. At the end of a call whose
//! body started, the target is frozen, and its `out` is collected there too.

use std::collections::BTreeMap;
use std::time::Duration;
use std::{fs, io};

use decision::{Event, JobId};
use gateway::{Put, Session};
use schema::ir::{self, AbsolutePath, Argv, Name};
use schema::sandbox::op::{CommandEnd, OpReply};
use schema::sandbox::tree::Limits;
use schema::sandbox::{
    Launch, RESULT_MAX_BYTES, RelativePath, command_env, dir_mode, file_mode, log_dir, out_dir,
    pumpkin_dir,
};
use schema::workflow::{self, Final, JobFunction, TargetId};
use serde_json::{Map, Value};
use stdx::{Context, Deadline};

use crate::child::{Child, Report};
use crate::host::deadline;
use crate::lease::command_dir;
use crate::record::{Stored, reference};
use crate::relay::{Relay, Waited};
use crate::sandbox::collect;
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

    /// Calls the final call's body `body`, given `keywords`, with a handle on `target` and the
    /// payload, on the calling thread, whose stack is [`stack_bytes`](Self::stack_bytes).
    ///
    /// # Errors
    ///
    /// As [`job`](Self::job)'s.
    fn final_call(
        &self,
        body: &ir::FinalBody,
        keywords: &Map<String, Value>,
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
    /// Proposes that the call's payload become the run's selection, replacing the last.
    fn select(&mut self);

    /// Proposes that the caller's work end, its selection locked.
    fn end_work(&mut self);
}

/// The target of a job's call, as its body drives it. A method's `Err` is why it failed, which
/// ends the call.
pub trait Target {
    /// Puts the call's payload at `path`, one file, its parents made.
    fn put(&mut self, path: &AbsolutePath) -> Result<(), String>;

    /// Runs `argv`, and waits for it to end, whatever its exit status.
    fn run(&mut self, argv: &Argv) -> Result<(), String>;

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
    pub(crate) payload: Stored,
    pub(crate) staged: Vec<(AbsolutePath, Stored)>,
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
    Final(&'r Final),
}

impl Of<'_> {
    pub(crate) fn target(&self) -> TargetId {
        match self {
            Self::Function { function, .. } => function.target,
            Self::Final(final_call) => final_call.target,
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
    child: &mut Child<'_, H, OpReply>,
    session: &mut Session,
) -> Ended {
    if child.stopped() {
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
        runs_count: 0,
        gets_count: 0,
    };
    let staged = call
        .staged
        .iter()
        .try_for_each(|(path, stored)| target.put_stored(path, stored));
    let returned = staged.and_then(|()| match call.of {
        Of::Function {
            function,
            arguments,
        } => bodies.job(&function.body, &function.keywords, arguments, &mut target),
        Of::Final(final_call) => {
            bodies.final_call(&final_call.body, &final_call.keywords, &mut target)
        }
    });
    target.end();
    match (target.cut, returned) {
        (Some((Cut::TimedOut, _)), _) => Ended::TimedOut,
        (Some((Cut::Stopped, _)), _) => Ended::Stopped,
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

/// The target of a body's call, on its child's session.
struct OnTarget<'a, 'c, 'r, H: Host> {
    call: &'a Call<'c>,
    child: &'a mut Child<'r, H, OpReply>,
    session: &'a mut Session,
    /// When the call's timeout passes, as a time since the run started.
    timeout: Duration,
    relay: Relay,
    /// What cut the call short, after which every method fails, and the command that ran then, if
    /// one did.
    cut: Option<(Cut, Option<u64>)>,
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
        if let Some((cut, _)) = self.cut {
            return Err(cut.reason());
        }
        let by = deadline(&self.child.clock, self.timeout);
        let idle = (self.relay).idle(self.child, self.call.job, (self.session, by));
        match idle {
            Ok(true) => return Err(self.cut_short(Cut::Stopped, None)),
            Ok(false) => {}
            Err(reason) => return Err(format!("the target's session ended: {reason}")),
        }
        if self.child.clock.now() >= self.timeout {
            return Err(self.cut_short(Cut::TimedOut, None));
        }
        Ok(by)
    }

    /// Cuts the call short while `running`, the command that runs then, if one does, reports a
    /// timeout, and returns why the method fails.
    fn cut_short(&mut self, cut: Cut, running: Option<u64>) -> String {
        if let Cut::TimedOut = cut {
            self.child.event(Event::CallTimedOut { job: self.call.job });
        }
        self.cut = Some((cut, running));
        cut.reason()
    }

    /// `done`, what came of an ask of the target, whose failure once the call's timeout, its
    /// deadline, has passed is the timeout's.
    fn timely<T>(&mut self, done: Result<T, String>) -> Result<T, String> {
        match done {
            Err(_) if self.child.clock.now() >= self.timeout => {
                Err(self.cut_short(Cut::TimedOut, None))
            }
            done => done,
        }
    }

    /// Logs what the body asked of the target, now.
    fn log(&self, targeted: Targeted) {
        let at = self.child.clock.now();
        self.child.log(Entry::Target(self.call.job, targeted, at));
    }

    /// Collects `dir`, a command's output, into the job's directory of the record, by `by`, or
    /// notes why it couldn't: a call doesn't fail for want of its log.
    fn collect_output(&mut self, dir: &RelativePath, by: Deadline) {
        let into = self.child.dir.join(log_dir().as_str());
        let collected = fs::create_dir_all(&into).map_err(|error| error.to_string());
        let collected = collected.and_then(|()| collect(self.session, dir, &self.child.dir, by));
        if let Err(error) = collected {
            let job = self.call.job.0;
            self.child
                .note(format!("job {job}'s {dir} is incomplete: {error}"));
        }
    }

    /// At the call's end, freezes the target, so that nothing it runs changes what is collected,
    /// then collects the output of the command that ran as the call was cut short, if one did,
    /// and the target's `out`, into the job's directory of the record, by the child's collect
    /// deadline. Notes what failed, which never fails the call, and collects all the same after
    /// a failed freeze; but collects nothing from a session that has ended.
    fn end(&mut self) {
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
        if let Some((_, Some(command))) = self.cut {
            self.collect_output(&command_dir(command), by);
        }
        if let Err(error) = collect(self.session, &out_dir(), &self.child.dir, by) {
            (self.child).note(format!("job {job}'s out is incomplete: {error}"));
        }
    }
}

impl<H: Host> OnTarget<'_, '_, '_, H> {
    fn put_stored(&mut self, path: &AbsolutePath, stored: &Stored) -> Result<(), String> {
        let by = self.going()?;
        let Some((dir, name)) = path.split_last() else {
            return Err("`/` is the sandbox's root: name a file's path".to_owned());
        };
        let payload = stored.read()?;
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
}

impl<H: Host> Target for OnTarget<'_, '_, '_, H> {
    fn put(&mut self, path: &AbsolutePath) -> Result<(), String> {
        self.put_stored(path, &self.call.payload)
    }

    fn run(&mut self, argv: &Argv) -> Result<(), String> {
        let by = self.going()?;
        let target = self.call.target;
        let command = self.runs_count;
        let dir = command_dir(command);
        self.runs_count += 1;
        let put = (self.session).put(&pumpkin_dir(), [Put::Dir(dir.clone(), dir_mode())], by);
        self.timely(put.context(format_args!("making {dir} failed")))?;
        let output = |name: RelativePath| pumpkin_dir().join(&dir.join(&name));
        let launch = Launch {
            argv: argv.clone(),
            env: command_env(&target.env),
            cwd: target.cwd.clone(),
            stdin: None,
            stdout: output(const { RelativePath::from_static("command.stdout") }),
            stderr: output(const { RelativePath::from_static("command.stderr") }),
        };
        let started = self.session.start(launch, by);
        self.timely(started.context("the command didn't start"))?;
        let argv = argv.clone();
        self.log(Targeted::Run { command, argv });
        let job = self.call.job;
        let waited = (self.relay).wait(self.child, job, self.timeout, self.session);
        match waited {
            Ok(Waited::Exited(status)) => {
                let end = CommandEnd::Exited(status);
                self.log(Targeted::Ended { command, end });
                self.collect_output(&dir, deadline(&self.child.clock, self.timeout));
                Ok(())
            }
            Ok(Waited::TimedOut) => Err(self.cut_short(Cut::TimedOut, Some(command))),
            Ok(Waited::Stopped) => Err(self.cut_short(Cut::Stopped, Some(command))),
            Err(reason) => Err(format!("the target's session ended: {reason}")),
        }
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
