//! The conformance suite every [`Provider`] passes, through the trait alone: one [`check`].
//!
//! Its outcomes are the same for every provider that keeps the trait's contracts, and each
//! provider's own test binary runs it on a [`Subject`] of its own. What every provider shares
//! is tested here; a provider's own tests cover only what its platform adds, and the tests of
//! `sandboxd` that need a real sandbox, which run on Docker.
//!
//! Where a contract lets providers differ, as [`Provider::list`]'s lets one leave out an ended
//! sandbox that it has freed itself, the suite shows what the contract fixes, by what the
//! subject's platform holds ([`Subject::held`]), so the outcomes stay the same.

#![expect(
    clippy::print_stderr,
    reason = "a run that can't be cleaned up says so"
)]

use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, thread};

use expect_test::expect;
use gateway::{Arrival, Put, Registration, Session, Sockets};
use provider::{
    Console, Create, ImageRef, Listed, Meeting, Provider, Role, RunTag, SandboxId, SandboxdRef,
    UnixTime,
};
use schema::ir::{AbsolutePath, Image, Name, Resources, User};
use schema::sandbox::{RelativePath, file_mode};
use stdx::{Context, Deadline};
use tempfile::TempDir;

/// A provider under the suite: the platform it reaches, how that platform makes a sandbox's
/// `sandboxd` misbehave, and what the platform holds.
pub trait Subject {
    /// A new instance of the provider, connected to the platform as a run connects. The suite
    /// makes each call on an instance of its own, as [`Provider`]'s rule on its instances
    /// allows.
    ///
    /// # Errors
    ///
    /// Fails if the platform can't be reached.
    fn connect(&self) -> Result<Box<dyn Provider + '_>, String>;

    /// What the suite publishes, which may need a file in `dir`, a directory the suite removes.
    fn locals(&self, dir: &Path) -> Locals;

    /// Makes sandbox `id`'s `sandboxd` stop reading its session before it answers another
    /// request.
    fn stall(&self, id: &SandboxId);

    /// The sandboxes of `run` that the platform holds, as it shows them itself rather than
    /// through the provider: those in which anything still runs, or whose resources are still
    /// to free.
    fn held(&self, run: &RunTag) -> Vec<SandboxId>;
}

/// What the suite publishes, which only a subject's platform can name.
pub struct Locals {
    /// The role's image, which every sandbox of the suite runs.
    pub image: Image,
    /// A `sandboxd` that dials.
    pub sandboxd: PathBuf,
    /// A `sandboxd` that exits without dialing.
    pub mute: PathBuf,
    /// A `sandboxd` that its sandbox can't run, so that its create fails, or its sandbox ends.
    pub broken: PathBuf,
    /// An image that the provider can't reach, such as one that nothing holds.
    pub unreachable_image: Image,
    /// A `sandboxd` that the provider can't reach, such as a path that names no file.
    pub unreachable_sandboxd: PathBuf,
}

/// A wait that nothing can end sooner, such as a put to a `sandboxd` that has stopped reading,
/// no longer than any provider's startup, so that no outcome rests on its length.
fn soon() -> Deadline {
    Deadline::after(Duration::from_secs(1))
}

/// Runs the suite against each of `subjects` and checks that their outcomes are the same.
///
/// Each runs in a run its name tags and a temporary directory of its own; the outcomes are those of
/// every provider that keeps [`Provider`]'s contracts. A test binary calls it once, with every
/// subject: `UPDATE_EXPECT=1` can't refill its snapshot from a second call in the same process.
///
/// # Panics
///
/// Panics if `subjects` is empty, an outcome differs, or a call the suite expects to succeed fails.
pub fn check(subjects: &[(&str, &dyn Subject)]) {
    let mut outcomes = (subjects.iter()).map(|&(name, subject)| (name, run(name, subject)));
    let Some((first, lines)) = outcomes.next() else {
        panic!("the batch of subjects is empty: name one");
    };
    for (name, other) in outcomes {
        assert!(
            other == lines,
            "the outcomes on {name} differ from those on {first}: expected every subject's to be \
             the same\n{name}'s:\n{other}\n{first}'s:\n{lines}"
        );
    }
    expect![[r#"
        publishing an image it can't reach: refused
        publishing a `sandboxd` it can't reach: refused
        lifecycle: accepted and set up by its startup
        listed: ["lifecycle"]
        listed as another run's: []
        deleting it as another run's: failed, naming ["lifecycle"]
        its console: read
        its console, as another run's: refused
        listed: ["lifecycle"]
        deleting a namesake as its own run's: done
        listed: ["lifecycle"]
        deleting another namesake, an id no sandbox has and it, at once: failed, naming ["lifecycle's namesake"]
        listed: []
        deleting that namesake as its own run's: done
        deleting it again, and an id no sandbox has: done
        mute: listed: ["mute (ended)"]
        deleting it: done
        stalled: accepted and set up by its startup
        then a put: TimedOut: the sandbox didn't take what was sent before the request's deadline: expected it to read while the deadline lasts
        listed: ["stalled"]
        deleting it: done
        listed: []
        broken: the run's list finds whatever its create left
        deleting what it left: done
        late: its create failed; the run's list finds whatever its create left
        deleting what it left: done"#]].assert_eq(&lines);
}

/// The suite's outcomes on `subject`, one to a line, from a run that `name` tags, which it cleans
/// up before it returns.
fn run(name: &str, subject: &dyn Subject) -> String {
    let mut suite = Suite::new(name, subject);
    let Locals {
        image,
        sandboxd,
        mute,
        broken,
        unreachable_image,
        unreachable_sandboxd,
    } = subject.locals(suite.dir.path());
    let mut lines = vec![
        format!(
            "publishing an image it can't reach: {}",
            published(&suite.connect().publish_image(&unreachable_image))
        ),
        format!(
            "publishing a `sandboxd` it can't reach: {}",
            published(&suite.connect().publish_sandboxd(&unreachable_sandboxd))
        ),
    ];
    // Each publish is a call on an instance of its own, gone before any create, as each create's
    // is: so every create takes what another instance published.
    let ours = suite.publish(&image, &sandboxd);
    // Another run's publish of the same, which the platform holds already.
    let theirs = suite.publish(&image, &sandboxd);
    let (image, sandboxd) = ours;
    let mute = suite.connect().publish_sandboxd(&mute).unwrap();
    let broken = suite.connect().publish_sandboxd(&broken).unwrap();
    lines.extend(lifecycle(&mut suite, &image, &sandboxd, &theirs));
    lines.extend(exits(&mut suite, &image, &mute));
    lines.extend(stalls(&mut suite, &image, &sandboxd));
    lines.extend(fails(&mut suite, &image, &broken));
    lines.extend(late(&mut suite, &image, &sandboxd));
    lines.join("\n")
}

/// A sandbox started, listed, read and deleted, which another run can neither list, read nor
/// delete, though it can create one of the same name, a namesake, from what it published itself
/// (`theirs`), and delete it, leaving the sandbox. Each id of a delete is tried on its own: a
/// delete of a namesake, an id no sandbox has and the sandbox, at once, fails, naming the
/// namesake, which it leaves for its own run to delete, yet deletes the sandbox; and deleting the
/// sandbox again, with an id no sandbox has, succeeds.
fn lifecycle(
    suite: &mut Suite<'_>,
    image: &ImageRef,
    sandboxd: &SandboxdRef,
    theirs: &(ImageRef, SandboxdRef),
) -> Vec<String> {
    let none = SandboxId::from("pumpkin-none".to_owned());
    let (id, session) = suite.start("lifecycle", image, sandboxd);
    let other = suite.other.clone();
    // Each line makes its calls as it is computed, so the lines' order is the calls'.
    let mut lines = vec![
        format!("lifecycle: {}", outcome(&session)),
        suite.listed(),
        format!(
            "listed as another run's: {:?}",
            suite.named(ids(suite.connect().list(&other).unwrap()))
        ),
        format!(
            "deleting it as another run's: {}",
            suite.delete(&other, std::slice::from_ref(&id))
        ),
        format!(
            "its console: {}",
            read(&suite.connect().console(&suite.run, &id))
        ),
        format!(
            "its console, as another run's: {}",
            read(&suite.connect().console(&other, &id))
        ),
        suite.listed(),
    ];
    // Each namesake's registration is kept until the namesake is deleted, so that its `sandboxd`
    // waits there to be accepted rather than end: to a delete, an ended sandbox that a provider
    // has freed itself would be no sandbox, not another run's.
    let (namesake, _first) = suite.namesake("lifecycle", &theirs.0, &theirs.1);
    lines.extend([
        format!(
            "deleting a namesake as its own run's: {}",
            suite.delete(&other, &[namesake])
        ),
        suite.listed(),
    ]);
    let (namesake, _second) = suite.namesake("lifecycle", &theirs.0, &theirs.1);
    // The run's own sandbox comes last, so that a delete that stops at a failure leaves it.
    let mixed = [namesake.clone(), none.clone(), id.clone()];
    lines.extend([
        format!(
            "deleting another namesake, an id no sandbox has and it, at once: {}",
            suite.delete_ours(&mixed)
        ),
        suite.listed(),
        format!(
            "deleting that namesake as its own run's: {}",
            suite.delete(&other, &[namesake])
        ),
        format!(
            "deleting it again, and an id no sandbox has: {}",
            suite.delete_ours(&[id, none])
        ),
    ]);
    lines
}

/// A sandbox whose `sandboxd` exits without dialing: by its startup the run's lists find it
/// ended, and it is deleted.
fn exits(suite: &mut Suite<'_>, image: &ImageRef, mute: &SandboxdRef) -> Vec<String> {
    let startup = suite.startup();
    let (id, _) = suite
        .create("mute", image, mute, from_now(startup))
        .unwrap();
    suite.await_ended(&Deadline::after(startup));
    vec![
        format!("mute: {}", suite.listed()),
        format!("deleting it: {}", suite.delete_ours(&[id])),
    ]
}

/// A sandbox whose `sandboxd` stalls: a request then times out, the run's list finds it still
/// running, as it hasn't exited, and it is deleted.
fn stalls(suite: &mut Suite<'_>, image: &ImageRef, sandboxd: &SandboxdRef) -> Vec<String> {
    let (id, session) = suite.start("stalled", image, sandboxd);
    let started = format!("stalled: {}", outcome(&session));
    suite.subject.stall(&id);
    vec![
        started,
        format!("then a put: {}", put(session.unwrap().0)),
        suite.listed(),
        format!("deleting it: {}", suite.delete_ours(&[id])),
        suite.listed(),
    ]
}

/// A create whose `sandboxd` can't run, which fails at once or makes a sandbox that ends: by its
/// startup, the run's list finds whatever it left that the platform holds ([`Creator::create`](provider::Creator::create)).
fn fails(suite: &mut Suite<'_>, image: &ImageRef, broken: &SandboxdRef) -> Vec<String> {
    let startup = suite.startup();
    // A provider may return from a create before its sandbox starts, so only what the create
    // left is checked, not whether it failed.
    let _ = suite.create("broken", image, broken, from_now(startup));
    suite.await_ended(&Deadline::after(startup));
    let [found, deleted] = suite.left();
    vec![format!("broken: {found}"), deleted]
}

/// A create whose deadline has passed, which fails: the run's list finds whatever it left that the
/// platform holds ([`Creator::create`](provider::Creator::create)).
fn late(suite: &mut Suite<'_>, image: &ImageRef, sandboxd: &SandboxdRef) -> Vec<String> {
    // The Unix epoch, a deadline long past.
    let created = suite.create("late", image, sandboxd, UnixTime::of(UNIX_EPOCH).unwrap());
    let [found, deleted] = suite.left();
    let failed = match created {
        Ok((id, _)) => format!("created, as {}", id.as_str()),
        Err(_) => "its create failed".to_owned(),
    };
    vec![format!("late: {failed}; {found}"), deleted]
}

/// Whether a publish succeeded or was refused, whatever its error says.
fn published<T>(published: &Result<T, String>) -> &'static str {
    match published {
        Ok(_) => "published",
        Err(_) => "refused",
    }
}

/// Whether a console was read or refused, whatever it holds.
fn read(console: &Result<Console, String>) -> &'static str {
    // A provider keeps a console as its platform does (`Console`), and a platform may keep it
    // late, as a cloud machine's serial console is a snapshot the platform takes when it
    // chooses: it need not yet hold what `sandboxd` printed, so only its read is checked.
    match console {
        Ok(_) => "read",
        Err(_) => "refused",
    }
}

/// Whether a sandbox's session was accepted and set up, or how that failed.
fn outcome<T>(session: &Result<T, String>) -> String {
    match session {
        Ok(_) => "accepted and set up by its startup".to_owned(),
        Err(error) => error.clone(),
    }
}

/// How a put of more than a socket's buffers hold fails, or that it succeeds, by [`soon`].
fn put(mut session: Session) -> String {
    // Many times what a Unix socket holds unread with the default buffers
    // (`net.core.wmem_default`, about 200 KiB), which the suite assumes of its host: buffers
    // that held it all would let the put succeed.
    let (path, bytes) = (
        RelativePath::try_from("big".to_owned()).unwrap(),
        vec![0; 4 << 20],
    );
    let home = AbsolutePath::try_from("/pumpkin/home".to_owned()).unwrap();
    match session.put(&home, &[Put::File(&path, file_mode(), &bytes)], soon()) {
        Ok(()) => "put".to_owned(),
        Err(error) => format!("{:?}: {error}", error.kind()),
    }
}

/// The suite's run on one subject, another run's tag, and a directory for what it publishes
/// and the sockets its sandboxes dial. Dropping it deletes what each run's list finds, and
/// removes the directory.
struct Suite<'a> {
    subject: &'a dyn Subject,
    /// Where the provider's sandboxes meet the gateway.
    sockets: Sockets,
    run: RunTag,
    other: RunTag,
    /// The sandboxes created, each by its id, its role, by which the outcomes name it, and
    /// whether the suite has deleted it.
    made: Vec<(SandboxId, &'static str, bool)>,
    /// The other run's sandboxes created, each by its id and the role whose name it shares, by
    /// which the outcomes name it, as that role's namesake.
    namesakes: Vec<(SandboxId, &'static str)>,
    /// The startup of the provider's slowest class, which bounds each create.
    startup: Duration,
    /// Last, so that it is removed once the sockets in it are.
    dir: TempDir,
}

impl<'a> Suite<'a> {
    fn new(name: &str, subject: &'a dyn Subject) -> Self {
        let tag = format!("pk-{}-{name}", std::process::id());
        // Private to this user, as the parent of every socket directory must be.
        let dir = tempfile::Builder::new()
            .prefix("suite-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let provider = subject.connect().unwrap();
        let classes = provider.ready().classes.values();
        let startup = classes.map(|class| class.startup_seconds).max().unwrap();
        Self {
            subject,
            sockets: Sockets::create(dir.path().join("sockets")).unwrap(),
            run: RunTag::try_from(tag.clone()).unwrap(),
            other: RunTag::try_from(format!("{tag}-other")).unwrap(),
            made: Vec::new(),
            namesakes: Vec::new(),
            startup: Duration::from_secs(startup.get()),
            dir,
        }
    }

    /// A new instance of the subject's provider, for one call.
    fn connect(&self) -> Box<dyn Provider + 'a> {
        self.subject.connect().unwrap()
    }

    /// What publishing `image` and `sandboxd`, each by a call of its own, returns.
    fn publish(&self, image: &Image, sandboxd: &Path) -> (ImageRef, SandboxdRef) {
        let image = self.connect().publish_image(image).unwrap();
        (image, self.connect().publish_sandboxd(sandboxd).unwrap())
    }

    /// Creates a sandbox of the run for `role`, named for it, which runs `image` and `sandboxd`,
    /// by `by`, and returns its id and the registration of where it dials, or how its create
    /// failed.
    fn create(
        &mut self,
        role: &'static str,
        image: &ImageRef,
        sandboxd: &SandboxdRef,
        by: UnixTime,
    ) -> Result<(SandboxId, Registration), String> {
        let (id, registration) = self.make(&self.run, role, role, (image, sandboxd), by)?;
        self.made.push((id.clone(), role, false));
        Ok((id, registration))
    }

    /// Creates a sandbox of the other run for `role`, named as one of the run's is, which runs
    /// `image` and `sandboxd`, by the startup of the provider's slowest class, and returns its id
    /// and the registration of where it dials.
    fn namesake(
        &mut self,
        role: &'static str,
        image: &ImageRef,
        sandboxd: &SandboxdRef,
    ) -> (SandboxId, Registration) {
        let socket = format!("other-{role}-{}", self.namesakes.len());
        let by = from_now(self.startup());
        let made = self.make(&self.other, role, &socket, (image, sandboxd), by);
        let (id, registration) = made.unwrap();
        self.namesakes.push((id.clone(), role));
        (id, registration)
    }

    /// Creates a sandbox of `run` for `role`, named for it, which
    /// runs `image` and `sandboxd`, meets the gateway where the provider's meeting says, at the
    /// socket the suite's sockets issue as `socket`, and gives up by
    /// `by`; returns its id and the registration of where it meets the gateway, or how its create
    /// failed.
    fn make(
        &self,
        run: &RunTag,
        role: &str,
        socket: &str,
        published: (&ImageRef, &SandboxdRef),
        by: UnixTime,
    ) -> Result<(SandboxId, Registration), String> {
        let create = asked(run, role, published, by);
        let provider = self.connect();
        match provider.meeting() {
            Meeting::Local(creator) => {
                let socket = Name::try_from(socket.to_owned()).unwrap();
                let (dial, registration) = self.sockets.issue(&socket).unwrap();
                Ok((creator.create(&create, dial)?.id, registration))
            }
        }
    }

    /// The startup of the provider's slowest class.
    fn startup(&self) -> Duration {
        self.startup
    }

    /// Whether the run's list finds all that the platform holds of the run, which, once the
    /// suite has deleted every sandbox it made, is what a create left; then what deleting what
    /// the list finds returns.
    fn left(&mut self) -> [String; 2] {
        let left = self.subject.held(&self.run);
        let listed = ids(self.connect().list(&self.run).unwrap());
        let found = match left.iter().filter(|id| !listed.contains(id)).count() {
            0 => "the run's list finds whatever its create left".to_owned(),
            missed => format!("the run's list misses {missed} sandboxes its create left"),
        };
        [
            found,
            format!("deleting what it left: {}", self.delete_ours(&listed)),
        ]
    }

    /// Waits until the run's list finds none of its sandboxes running, or `by` has passed.
    fn await_ended(&self, by: &Deadline) {
        let running = |listed: Vec<Listed>| listed.iter().any(|listed| !listed.ended);
        // A sandbox whose `sandboxd` can't serve may still run as its create returns.
        while running(self.connect().list(&self.run).unwrap()) && by.remaining().is_some() {
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// Creates a sandbox for `role`, as [`create`](Self::create) does, and accepts its session,
    /// set up, by the startup of the provider's slowest class, from the create's start.
    fn start(
        &mut self,
        role: &'static str,
        image: &ImageRef,
        sandboxd: &SandboxdRef,
    ) -> (SandboxId, Result<(Session, Receiver<Arrival>), String>) {
        let startup = self.startup();
        let by = Deadline::after(startup);
        let (id, registration) = (self.create(role, image, sandboxd, from_now(startup))).unwrap();
        // Nothing stops the start, whose `sandboxd` dials.
        let (sender, arrivals) = mpsc::channel();
        let accepted = registration.accept(by, None, false, sender, |arrival| arrival, || false);
        let accepted = accepted.map_err(|error| error.to_string());
        let session = accepted.and_then(|mut session| {
            session
                .set_up(by)
                .context(format_args!("sandbox {}", id.as_str()))?;
            Ok((session, arrivals))
        });
        (id, session)
    }

    /// What deleting `ids` from the run returns, as [`delete`](Self::delete) shows it; the
    /// suite counts each as deleted from then on.
    fn delete_ours(&mut self, ids: &[SandboxId]) -> String {
        let run = self.run.clone();
        let deleted = self.delete(&run, ids);
        for (id, _, deleted) in &mut self.made {
            *deleted |= ids.contains(id);
        }
        deleted
    }

    /// Whether deleting `ids` as `run`'s, one of the suite's two runs, is done, or fails, and
    /// then which of `ids` its error names; then those of `ids` whose sandboxes the platform held
    /// before the delete and, against [`Provider::delete`]'s contract, still
    /// holds after it, of `run`, or no longer holds, of the other run, each [named](Self::named).
    fn delete(&self, run: &RunTag, ids: &[SandboxId]) -> String {
        let runs = [&self.run, &self.other];
        let held = || runs.map(|run| self.subject.held(run));
        let before = held();
        let deleted = self.connect().delete(run, ids);
        let after = held();
        let mut shown = match deleted {
            Ok(()) => "done".to_owned(),
            Err(error) => {
                let named = ids.iter().filter(|id| error.contains(id.as_str()));
                format!("failed, naming {:?}", self.named(named.cloned().collect()))
            }
        };
        for ((of, before), after) in runs.into_iter().zip(before).zip(after) {
            let asked = of.as_str() == run.as_str();
            let broken =
                (ids.iter()).filter(|id| before.contains(id) && after.contains(id) == asked);
            let broken = self.named(broken.cloned().collect());
            if !broken.is_empty() {
                let holds = if asked {
                    "still holds"
                } else {
                    "no longer holds"
                };
                let _ = write!(shown, ", but the platform {holds} {broken:?}");
            }
        }
        shown
    }

    /// What the run's list finds, as the run takes it: each sandbox the suite created that the
    /// list finds, or that the suite hasn't deleted, [named](Self::named), marked ended if the
    /// list says so, or if it leaves the sandbox out once the platform no longer holds it, and
    /// marked left out if it leaves it out while the platform holds it, which
    /// [`Provider::list`] forbids; then any other sandbox the list finds.
    fn listed(&self) -> String {
        let listed = self.connect().list(&self.run).unwrap();
        let held = self.subject.held(&self.run);
        let mut shown = Vec::new();
        for (id, role, deleted) in &self.made {
            let found = listed.iter().find(|listed| listed.id == *id);
            let ended = found.map(|listed| listed.ended);
            match (ended, *deleted, held.contains(id)) {
                (Some(false), _, _) => shown.push((*role).to_owned()),
                (Some(true), _, _) | (None, false, false) => {
                    shown.push(format!("{role} (ended)"));
                }
                (None, false, true) => shown.push(format!("{role} (left out)")),
                (None, true, _) => {}
            }
        }
        let others = listed
            .iter()
            .filter(|listed| self.role(&listed.id).is_none());
        shown.extend(others.map(|Listed { id, ended }| match ended {
            true => format!("{} (ended)", id.as_str()),
            false => id.as_str().to_owned(),
        }));
        format!("listed: {:?}", sorted(shown.into_iter()))
    }

    /// Each id as the role of the sandbox it names, or as that role's namesake, if the suite
    /// created one, or else the id, sorted.
    fn named(&self, ids: Vec<SandboxId>) -> Vec<String> {
        let named = ids.into_iter().map(|id| {
            let namesake = (self.namesakes.iter()).find(|(namesake, _)| *namesake == id);
            match (self.role(&id), namesake) {
                (Some(role), _) => role.to_owned(),
                (None, Some((_, role))) => format!("{role}'s namesake"),
                (None, None) => id.as_str().to_owned(),
            }
        });
        sorted(named)
    }

    /// The role of the sandbox `id` names, if the suite created one.
    fn role(&self, id: &SandboxId) -> Option<&'static str> {
        (self.made.iter())
            .find(|(made, _, _)| made == id)
            .map(|(_, role, _)| *role)
    }
}

/// The create of a sandbox of `run` for `role`, named for it, which runs
/// `image` and `sandboxd` as [`suite_role`] says, and gives up by `by`.
fn asked(
    run: &RunTag,
    role: &str,
    (image, sandboxd): (&ImageRef, &SandboxdRef),
    by: UnixTime,
) -> Create {
    Create {
        run: run.clone(),
        name: Name::try_from(role.to_owned()).unwrap(),
        role: suite_role(role),
        image: image.clone(),
        sandboxd: sandboxd.clone(),
        ops: false,
        model: false,
        // Far past the suite's end, which comes within minutes on any provider.
        hard_end: from_now(Duration::from_hours(1)),
        by,
    }
}

/// Role `role`, as the suite's sandboxes have it: run as 4242:4243, with half a CPU and 256 MiB.
fn suite_role(role: &str) -> Role {
    Role {
        name: Name::try_from(role.to_owned()).unwrap(),
        user: serde_json::from_str::<User>(r#"{"uid": 4242, "gid": 4243}"#).unwrap(),
        resources: serde_json::from_str::<Resources>(
            r#"{"cpu_millis": 500, "memory_bytes": 268435456}"#,
        )
        .unwrap(),
    }
}

/// The time `after` from now.
fn from_now(after: Duration) -> UnixTime {
    UnixTime::of(SystemTime::now())
        .unwrap()
        .saturating_add(after)
}

/// `names`, sorted.
fn sorted(names: impl Iterator<Item = String>) -> Vec<String> {
    let mut names: Vec<String> = names.collect();
    names.sort();
    names
}

impl Drop for Suite<'_> {
    // An error here is printed, never panicked on, since a panic while a failed test unwinds
    // would abort the test binary and hide the other tests' results; only a bug panics.
    fn drop(&mut self) {
        for run in [&self.run, &self.other] {
            let deleted = self.subject.connect().and_then(|provider| {
                let listed = provider.list(run)?;
                provider.delete(run, &ids(listed))
            });
            if let Err(error) = deleted {
                eprintln!("run {} can't be cleaned up: {error}", run.as_str());
            }
        }
    }
}

/// The ids of the sandboxes a list found.
fn ids(listed: Vec<Listed>) -> Vec<SandboxId> {
    (listed.into_iter())
        .map(|Listed { id, ended: _ }| id)
        .collect()
}
