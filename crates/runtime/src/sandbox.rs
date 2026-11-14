//! The runtime's one start path, which every sandbox of a run starts on ([`Sandboxes::start`]),
//! which ends a start once its sandbox has ended, and what each role's sandbox needs of it: its IR
//! command launched, and what it wrote collected.
//!
//! The provider's side of each step is [`Provider`]'s, and the session's [`Session`]'s.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::io;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant, SystemTime};

use decision::JobId;
use gateway::{Arrival, Session, Sockets};
use provider::{
    Create, Created, Creator, ImageRef, Listed, Meeting, Provider, Role, RunTag, SandboxId,
    SandboxdRef, UnixTime,
};
use schema::ir::{Command, Image, Name};
use schema::sandbox::tree::Limits;
use schema::sandbox::{
    Launch, OpPayloadLimit, PUMPKIN_BYTES, RelativePath, SocketDial, command_env, command_stderr,
    command_stdout, pumpkin_dir,
};
use schema::workflow::{ByRole, JudgeId, Placed, RoleId, Workflow};
use stdx::{Context, Deadline};

/// The most entries a collected tree may hold.
const ENTRIES: u64 = 100_000;

/// What a sandbox's `/pumpkin` holds at most: anything collected from below it fits.
pub(super) const PUMPKIN: Limits = Limits {
    bytes: PUMPKIN_BYTES,
    entries: ENTRIES,
};

/// `role` of `workflow`, as a provider sees it.
fn provider_role(workflow: &Workflow, role: RoleId) -> Role {
    let sandbox = workflow.sandbox(role);
    Role {
        name: sandbox.name.clone(),
        user: sandbox.user,
        resources: sandbox.needs,
    }
}

/// What the sandboxes of a run run, published on its provider, and where they meet the gateway:
/// what the run makes of them before it starts ([`Published::create`]).
pub(super) struct Published<'p> {
    hosting: Hosting<'p>,
    /// Each role's image, as its provider published it.
    images: ByRole<ImageRef>,
    sockets: Sockets,
}

/// The sandboxes of one run of a workflow on its provider, once the run has started: what they
/// run, published, and where they meet the gateway.
pub(super) struct Sandboxes<'p> {
    run: RunTag,
    placed: &'p Placed,
    hosting: Hosting<'p>,
    /// Each role's image, as its provider published it.
    images: ByRole<ImageRef>,
    sockets: Sockets,
    /// How long a start waits between its lists of its sandbox's provider: a fortieth of the
    /// quickest startup of the run's roles, at most 15 s.
    between_looks: Duration,
}

/// The provider of the run, the `sandboxd` it published, which each of its creates names, and
/// where its sandboxes meet the gateway.
struct Hosting<'p> {
    provider: &'p dyn Provider,
    sandboxd: SandboxdRef,
    meets: Meets<'p>,
}

/// Where a provider's sandboxes meet the gateway, as its [`Meeting`] says, with its create.
#[derive(Clone, Copy)]
enum Meets<'p> {
    /// At a socket of each one's own, among the run's [`Sockets`].
    Sockets(&'p dyn Creator<SocketDial>),
}

impl Meets<'_> {
    /// Creates the sandbox that `create` describes where it meets the gateway, issued among
    /// `sockets` if at a socket, and returns what the create made and the registration that
    /// accepts the sandbox's session.
    fn create(
        self,
        sockets: &Sockets,
        create: &Create,
    ) -> Result<(Created, gateway::Registration), String> {
        let issued = |error: io::Error| error.to_string();
        match self {
            Self::Sockets(creator) => {
                let (dial, registration) = sockets.issue(&create.name).map_err(issued)?;
                Ok((creator.create(create, dial)?, registration))
            }
        }
    }
}

/// One of a run's sandboxes, by which its start names it as no other sandbox of the run, and
/// finds its role.
#[derive(Clone, Copy)]
pub(super) enum Which {
    /// The agent's, `agent`, which a run starts once.
    Agent,
    /// Judge call `job`'s, `job-<number>`, of judge `judge`.
    Job(JobId, JudgeId),
}

impl Which {
    fn role(self, workflow: &Workflow) -> RoleId {
        match self {
            Self::Agent => workflow.agent().role,
            Self::Job(_, judge) => judge.role(),
        }
    }

    /// The sandbox's name within its run, which its socket directory has too.
    pub(super) fn name(self) -> Name {
        let name = match self {
            Self::Agent => "agent".to_owned(),
            Self::Job(job, _) => format!("job-{}", job.0),
        };
        // A `Name` made of such parts, which couldn't fail, would be a `pub` item of the schema
        // for this one caller.
        let Ok(name) = Name::try_from(name) else {
            unreachable!(
                "a sandbox's name within its run is no name, though `agent`, and `job-` with a \
                 number's digits, are names: a letter, then letters, digits and `-`, 24 at most"
            )
        };
        name
    }
}

/// A sandbox that started, with its session, set up.
pub(super) struct Started {
    id: SandboxId,
    pub(super) session: Session,
}

impl<'p> Published<'p> {
    /// Publishes what `placed`'s sandboxes run on `provider`: `sandboxd`, a static binary, then
    /// each image a role names, once; then makes the directory of run `run`'s sockets,
    /// `pumpkin-<run>` in the system's temporary directory, which must not exist yet.
    pub(super) fn create(
        provider: &'p dyn Provider,
        placed: &Placed,
        sandboxd: &Path,
        run: &RunTag,
    ) -> Result<Self, String> {
        let workflow = placed.workflow();
        let meets = match provider.meeting() {
            Meeting::Local(creator) => Meets::Sockets(creator),
        };
        let sandboxd =
            (provider.publish_sandboxd(sandboxd)).context("publishing `sandboxd` failed")?;
        let hosting = Hosting {
            provider,
            sandboxd,
            meets,
        };
        let mut published = BTreeMap::<Image, ImageRef>::new();
        let images =
            workflow.by_role(
                |(_, sandbox)| match published.entry(sandbox.image.clone()) {
                    Entry::Occupied(entry) => Ok(entry.get().clone()),
                    Entry::Vacant(entry) => {
                        let image = provider.publish_image(entry.key());
                        let image = image.context(format_args!(
                            "publishing role `{}`'s image failed",
                            sandbox.name
                        ))?;
                        Ok::<_, String>(entry.insert(image).clone())
                    }
                },
            )?;
        // In the system's temporary directory, where no other user may rename entries, as
        // `Sockets::create` needs.
        let sockets = std::env::temp_dir().join(format!("pumpkin-{}", run.as_str()));
        let sockets = Sockets::create(sockets).map_err(|error| error.to_string())?;
        Ok(Self {
            hosting,
            images,
            sockets,
        })
    }
}

impl<'p> Sandboxes<'p> {
    /// The sandboxes of run `run`, of `placed`, which start from what was `published` for them.
    pub(super) fn new(published: Published<'p>, run: RunTag, placed: &'p Placed) -> Self {
        let Published {
            hosting,
            images,
            sockets,
        } = published;
        // A start ends within a fortieth of its role's startup, or 15 s, and a list's time, of
        // its sandbox's end; so the looks follow the run's quickest startup.
        let between_looks = (placed.workflow().roles())
            .fold(Duration::from_secs(15), |between, (role, _)| {
                between.min(placed.startup(role) / 40)
            });
        Self {
            run,
            placed,
            hosting,
            images,
            sockets,
            between_looks,
        }
    }

    /// Starts `which` sandbox, its create given the run's `hard_end` ([`Create::hard_end`]), and
    /// accepts its session, set up, by `by`, its arrivals sent on `arrivals` as `wrap` makes them
    /// ([`Registration::accept`]); it makes ops of at most `ops`, and serves its role's model
    /// route, if it has one. A sandbox not started by then, or found ended before it connects
    /// ([`Sandboxes::ended`]), is deleted, its console quoted in the error, with why the last list
    /// or the delete failed.
    ///
    /// [`Registration::accept`]: gateway::Registration::accept
    pub(super) fn start<T: Send + 'static>(
        &self,
        which: Which,
        ops: Option<OpPayloadLimit>,
        by: Deadline,
        hard_end: UnixTime,
        arrivals: Sender<T>,
        wrap: impl Fn(Arrival) -> T + Send + 'static,
    ) -> Result<Started, String> {
        let name = which.name();
        // The start's deadline by the machine's clock, which the provider reads, at most the hard
        // end, as the run is abandoned then.
        let now = UnixTime::of(SystemTime::now())?;
        let by_unix = (by.remaining()).map_or(now, |left| now.saturating_add(left));
        let role = which.role(self.workflow());
        let model = self.workflow().sandbox(role).model_route.is_some();
        let create = Create {
            run: self.run.clone(),
            name,
            role: provider_role(self.workflow(), role),
            image: self.images.get(role).clone(),
            sandboxd: self.hosting.sandboxd.clone(),
            ops: ops.is_some(),
            model,
            hard_end,
            by: by_unix.min(hard_end),
        };
        let (Created { id }, registration) = self.hosting.meets.create(&self.sockets, &create)?;
        let looks = Looks::new();
        let stopped = || looks.ended(self.between_looks, || self.ended(&id));
        let accepted = registration.accept(by, ops, model, arrivals, wrap, stopped);
        let accepted = accepted.map_err(|error| match looks.found() {
            (true, _) => "it ended before it met the gateway: expected its `sandboxd` to run \
                          until it connects"
                .to_owned(),
            (false, Some(failed)) if error.kind() == io::ErrorKind::TimedOut => {
                format!("{error}; the last list of its provider failed: {failed}")
            }
            (false, _) => error.to_string(),
        });
        let session = accepted.and_then(|mut session| session.set_up(by).map(|()| session));
        match session {
            Ok(session) => Ok(Started { id, session }),
            Err(error) => Err(self.unstarted(&id, &error)),
        }
    }

    /// Whether the provider's list of the run finds sandbox `id` ended: a list leaves it out only
    /// once it is deleted, or has ended and been freed ([`Provider::list`]), so only a list that
    /// finds it running says it hasn't; or why the list failed.
    fn ended(&self, id: &SandboxId) -> Result<bool, String> {
        let listed = self.hosting.provider.list(&self.run)?;
        Ok(!(listed.iter()).any(|listed| listed.id == *id && !listed.ended))
    }

    /// Why sandbox `id`, whose session wasn't set up for `error`, didn't start: `error`, the
    /// end of its console, and whether deleting it failed, which this tries.
    fn unstarted(&self, id: &SandboxId, error: &str) -> String {
        let provider = self.hosting.provider;
        let mut parts = vec![format!("sandbox {}: {error}", id.as_str())];
        match provider.console(&self.run, id) {
            Ok(console) if console.text.trim().is_empty() => {}
            Ok(console) => parts.push(format!("its console ends: {}", console.text.trim())),
            Err(error) => parts.push(format!("its console can't be read: {error}")),
        }
        let deleted = provider.delete(&self.run, std::slice::from_ref(id));
        if let Err(error) = deleted {
            parts.push(format!("deleting it failed: {error}"));
        }
        parts.join("; ")
    }

    pub(super) fn workflow(&self) -> &'p Workflow {
        self.placed.workflow()
    }

    /// Deletes a sandbox that started, then ends its session.
    pub(super) fn delete(&self, started: Started) -> Result<(), String> {
        let Started { id, session } = started;
        let deleted = (self.hosting.provider).delete(&self.run, std::slice::from_ref(&id));
        drop(session);
        deleted.context(format_args!("deleting sandbox {} failed", id.as_str()))
    }

    /// Deletes every sandbox of the run that its provider still lists, and returns their ids.
    pub(super) fn sweep(&self) -> Result<Vec<SandboxId>, String> {
        let provider = self.hosting.provider;
        let listed = (provider.list(&self.run)).context("listing the run's sandboxes failed")?;
        let ids: Vec<SandboxId> = (listed.into_iter())
            .map(|Listed { id, ended: _ }| id)
            .collect();
        if !ids.is_empty() {
            (provider.delete(&self.run, &ids))
                .context("deleting the sandboxes the run left failed")?;
        }
        Ok(ids)
    }
}

/// A start's looks for its sandbox's end, made from within its accept's wait, each once the time
/// between looks has passed since the last.
struct Looks {
    last: Cell<Instant>,
    /// Whether a look found the sandbox ended.
    ended: Cell<bool>,
    /// Why the last look's list failed, if it did.
    failed: RefCell<Option<String>>,
}

impl Looks {
    fn new() -> Self {
        Self {
            last: Cell::new(Instant::now()),
            ended: Cell::new(false),
            failed: RefCell::new(None),
        }
    }

    /// Whether a look found the sandbox ended, looking with `look` if `between` has passed since
    /// the last look. The first comes `between` after the create returned, so every list speaks
    /// for the sandbox it made.
    fn ended(&self, between: Duration, look: impl FnOnce() -> Result<bool, String>) -> bool {
        if !self.ended.get() && self.last.get().elapsed() >= between {
            match look() {
                Ok(ended) => {
                    self.failed.replace(None);
                    self.ended.set(ended);
                }
                Err(error) => {
                    self.failed.replace(Some(error));
                }
            }
            self.last.set(Instant::now());
        }
        self.ended.get()
    }

    /// Whether a look found the sandbox ended, and why the last list failed, if it did.
    fn found(self) -> (bool, Option<String>) {
        (self.ended.get(), self.failed.into_inner())
    }
}

/// How to start `command` in a sandbox: as the IR says, in [`command_env()`]'s environment, with
/// an empty standard input, and its output at [`command_stdout()`] and [`command_stderr()`].
pub(super) fn launch(command: &Command) -> Launch {
    let Command { argv, env, cwd } = command;
    Launch {
        argv: argv.clone(),
        env: command_env(env),
        cwd: cwd.clone(),
        stdin: None,
        stdout: pumpkin_dir().join(&command_stdout()),
        stderr: pumpkin_dir().join(&command_stderr()),
    }
}

/// Collects the sandbox's `dir`, below [`pumpkin_dir()`], into the same path below `record`, a
/// new directory there, within `limits`, by `by`. After a failure, it holds what arrived before
/// it.
pub(super) fn collect(
    session: &mut Session,
    dir: &RelativePath,
    record: &Path,
    limits: Limits,
    by: Deadline,
) -> Result<(), String> {
    let from = pumpkin_dir().join(dir);
    session
        .collect(&from, &record.join(dir.as_str()), limits, by)
        .context(format_args!("collecting {from} failed"))
}
