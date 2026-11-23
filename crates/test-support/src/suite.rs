//! The contract of [`Provider`] as checks through the trait alone: one [`check`].
//!
//! Its outcomes are the same for every provider that keeps the trait's contracts. A provider's
//! tests run it on a [`Subject`] of its own where the provider can be run in a test. What every
//! provider shares is tested here; a provider's own tests cover only what its platform adds.
//!
//! Where a contract lets providers differ, as [`Sweep::list`](provider::Sweep::list)'s lets one
//! leave out an ended sandbox that it has freed itself, the suite shows what the contract fixes,
//! by what the subject's platform holds ([`Subject::held`]), or whether its provider holds
//! anything for runs ([`Subject::reserves`]), so the outcomes stay the same.

#![expect(
    clippy::print_stderr,
    reason = "a run that can't be cleaned up says so"
)]

use std::fmt::Write as _;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use expect_test::expect;
use gateway::{Arrival, Dialer, Put, Registration, Session, Sockets};
use provider::{
    Console, Create, ImageRef, Listed, Meeting, Provider, Reservation, Role, RunTag, SandboxId,
    SandboxdRef, UnixTime,
};
use schema::ir::{AbsolutePath, Argv, Env, Image, Name, Resources, User};
use schema::sandbox::{Launch, RelativePath, file_mode};
use stdx::{Context, Deadline};

use crate::TestDir;

/// A provider under the suite: the platform it reaches, how that platform makes a sandbox's
/// `sandboxd` misbehave, and what the platform holds.
pub trait Subject {
    /// A new instance of the provider, connected to the platform as a run connects. The suite makes
    /// each call on an instance of its own, as [`Provider`]'s rule on its instances allows.
    fn connect(&self) -> Result<Box<dyn Provider + '_>, String>;

    /// What the suite publishes, which may need a file in `dir`, a directory the suite removes.
    fn locals(&self, dir: &Path) -> Locals;

    /// Makes sandbox `id`'s `sandboxd` stop reading its session before it answers another
    /// request.
    fn stall(&self, id: &SandboxId);

    /// The sandboxes of `run` that the platform holds, as it shows them itself rather than through
    /// the provider: those in which anything still runs, or whose resources are still to free.
    fn held(&self, run: &RunTag) -> Vec<SandboxId>;

    /// Whether the provider shares a host between runs, and so refuses a create for a role its run
    /// holds nothing for ([`Provider::reserve`]).
    fn reserves(&self) -> bool;
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

// A wait that nothing can end sooner, such as a put to a `sandboxd` that has stopped reading, no
// longer than any provider's startup, so that no outcome rests on its length.
fn soon() -> Deadline {
    Deadline::after(Duration::from_secs(1))
}

// The roles of the suite's run, each of which it starts sandboxes of only once those of the one
// before are gone; the other run's is the first alone.
const ROLES: [&str; 6] = ["lifecycle", "killed", "mute", "stalled", "broken", "late"];

// How long the suite waits for room to reserve on its provider, which other tests may hold for as
// long as they run.
const ROOM: Duration = Duration::from_mins(10);

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
        a create for a role the run holds nothing for, where the provider holds anything: refused
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
        killed: its command exited: Signal(9)
        then a command: started
        deleting it: done
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
    // A provider that holds nothing for runs makes such a create as any other, so it isn't made.
    let refused = !subject.reserves()
        || (suite.create("unheld", &image, &sandboxd, from_now(suite.startup()))).is_err();
    lines.push(format!(
        "a create for a role the run holds nothing for, where the provider holds anything: {}",
        if refused { "refused" } else { "made" }
    ));
    lines.extend(lifecycle(&mut suite, &image, &sandboxd, &theirs));
    lines.extend(kills(&mut suite, &image, &sandboxd));
    lines.extend(exits(&mut suite, &image, &mute));
    lines.extend(stalls(&mut suite, &image, &sandboxd));
    lines.extend(fails(&mut suite, &image, &broken));
    lines.extend(late(&mut suite, &image, &sandboxd));
    lines.join("\n")
}

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
    // waits there to be accepted rather than end: to a delete, an ended sandbox that a provider has
    // freed itself would be no sandbox, not another run's.
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

fn kills(suite: &mut Suite<'_>, image: &ImageRef, sandboxd: &SandboxdRef) -> Vec<String> {
    let (id, started) = suite.start("killed", image, sandboxd);
    let (mut session, arrivals) = started.unwrap();
    let by = || crate::left(Deadline::after(suite.startup()));
    let process = session.start(sleep(), by()).unwrap();
    session.kill(process, by()).unwrap();
    let exited = match arrivals.recv_timeout(suite.startup()).unwrap() {
        Arrival::Exited {
            process: exited,
            status,
        } if exited == process => format!("{status:?}"),
        other => format!("{other:?}"),
    };
    let next = match session.start(sleep(), by()) {
        Ok(_) => "started".to_owned(),
        Err(error) => error.to_string(),
    };
    vec![
        format!("killed: its command exited: {exited}"),
        format!("then a command: {next}"),
        format!("deleting it: {}", suite.delete_ours(&[id])),
    ]
}

fn sleep() -> Launch {
    let path = |path: &str| AbsolutePath::try_from(path.to_owned()).unwrap();
    Launch {
        argv: Argv::try_from(vec!["sleep".to_owned(), "60".to_owned()]).unwrap(),
        env: Env::default(),
        cwd: path("/pumpkin/home"),
        stdin: None,
        stdout: path("/pumpkin/log/sleep.stdout"),
        stderr: path("/pumpkin/log/sleep.stderr"),
    }
}

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

fn fails(suite: &mut Suite<'_>, image: &ImageRef, broken: &SandboxdRef) -> Vec<String> {
    let startup = suite.startup();
    // A provider may return from a create before its sandbox starts, so only what the create left
    // is checked, not whether it failed.
    let _ = suite.create("broken", image, broken, from_now(startup));
    suite.await_ended(&Deadline::after(startup));
    let [found, deleted] = suite.left();
    vec![format!("broken: {found}"), deleted]
}

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

fn published<T>(published: &Result<T, String>) -> &'static str {
    match published {
        Ok(_) => "published",
        Err(_) => "refused",
    }
}

fn read(console: &Result<Console, String>) -> &'static str {
    // A provider keeps a console as its platform does (`Console`), and a platform may keep it late,
    // as a cloud machine's serial console is a snapshot the platform takes when it chooses: it need
    // not yet hold what `sandboxd` printed, so only its read is checked.
    match console {
        Ok(_) => "read",
        Err(_) => "refused",
    }
}

fn outcome<T>(session: &Result<T, String>) -> String {
    match session {
        Ok(_) => "accepted and set up by its startup".to_owned(),
        Err(error) => error.clone(),
    }
}

fn put(mut session: Session) -> String {
    // Many times what a Unix socket holds unread with the default buffers
    // (`net.core.wmem_default`), which the suite assumes of its host: buffers that held it all
    // would let the put succeed.
    let (path, bytes) = (
        RelativePath::try_from("big".to_owned()).unwrap(),
        vec![0; 4 << 20],
    );
    let home = AbsolutePath::try_from("/pumpkin/home".to_owned()).unwrap();
    match session.put(
        &home,
        [Put::File(path, file_mode(), &bytes)],
        crate::left(soon()),
    ) {
        Ok(()) => "put".to_owned(),
        Err(error) => format!("{:?}: {error}", error.kind()),
    }
}

struct Suite<'a> {
    subject: &'a dyn Subject,
    sockets: Sockets,
    dialer: Dialer,
    run: RunTag,
    other: RunTag,
    made: Vec<(SandboxId, &'static str, bool)>,
    namesakes: Vec<(SandboxId, &'static str)>,
    slowest: (Name, Duration),
    _held: [Reservation; 2],
    dir: TestDir,
}

impl<'a> Suite<'a> {
    fn new(name: &str, subject: &'a dyn Subject) -> Self {
        let tag = format!("pk-{}-{name}", std::process::id());
        let dir = TestDir::new();
        let provider = subject.connect().unwrap();
        let classes = provider.ready().classes.iter();
        let (class, declared) = classes
            .max_by_key(|(_, class)| class.startup_seconds)
            .unwrap();
        let slowest = (
            class.clone(),
            Duration::from_secs(declared.startup_seconds.get()),
        );
        let run = RunTag::try_from(tag.clone()).unwrap();
        let other = RunTag::try_from(format!("{tag}-other")).unwrap();
        let held = [(&run, &ROLES[..]), (&other, &ROLES[..1])]
            .map(|(run, roles)| reserved(&*provider, run, roles));
        Self {
            subject,
            sockets: Sockets::create(dir.path().join("sockets")).unwrap(),
            dialer: Dialer::new(crate::edge::client()),
            run,
            other,
            made: Vec::new(),
            namesakes: Vec::new(),
            slowest,
            _held: held,
            dir,
        }
    }

    fn connect(&self) -> Box<dyn Provider + 'a> {
        self.subject.connect().unwrap()
    }

    fn publish(&self, image: &Image, sandboxd: &Path) -> (ImageRef, SandboxdRef) {
        let image = self.connect().publish_image(image).unwrap();
        (image, self.connect().publish_sandboxd(sandboxd).unwrap())
    }

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

    fn make(
        &self,
        run: &RunTag,
        role: &str,
        socket: &str,
        published: (&ImageRef, &SandboxdRef),
        by: UnixTime,
    ) -> Result<(SandboxId, Registration), String> {
        let create = asked(run, role, self.slowest.0.clone(), published, by);
        let provider = self.connect();
        match provider.meeting() {
            Meeting::Local(creator) => {
                let socket = Name::try_from(socket.to_owned()).unwrap();
                let (dial, registration) = self.sockets.issue(&socket).unwrap();
                Ok((creator.create(&create, dial)?.id, registration))
            }
            Meeting::Listen(creator) => {
                let (dial, unreached) = self.dialer.issue().unwrap();
                let (created, endpoint) = creator.create(&create, dial)?;
                Ok((created.id, unreached.at(endpoint)))
            }
            Meeting::Private(_) => panic!(
                "the suite meets no sandbox that dials a private address: run it on a provider \
                 whose sandboxes dial a socket, or listen"
            ),
        }
    }

    fn startup(&self) -> Duration {
        self.slowest.1
    }

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

    fn await_ended(&self, by: &Deadline) {
        let running = |listed: Vec<Listed>| listed.iter().any(|listed| !listed.ended);
        // A sandbox whose `sandboxd` can't serve may still run as its create returns.
        while running(self.connect().list(&self.run).unwrap()) && by.remaining().is_some() {
            thread::sleep(Duration::from_millis(100));
        }
    }

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
        let accepted = registration.accept(|| by.remaining(), None, false, sender, |a| a, || false);
        let accepted = accepted.map_err(|error| error.to_string());
        let session = accepted.and_then(|mut session| {
            session
                .set_up(|| by.remaining())
                .context(format_args!("sandbox {}", id.as_str()))?;
            Ok((session, arrivals))
        });
        (id, session)
    }

    fn delete_ours(&mut self, ids: &[SandboxId]) -> String {
        let run = self.run.clone();
        let deleted = self.delete(&run, ids);
        for (id, _, deleted) in &mut self.made {
            *deleted |= ids.contains(id);
        }
        deleted
    }

    fn delete(&self, run: &RunTag, ids: &[SandboxId]) -> String {
        let runs = [&self.run, &self.other];
        let held = || runs.map(|run| self.subject.held(run));
        let before = held();
        let deleted = self.connect().delete(run, ids);
        let after = held();
        let mut shown = match deleted {
            Ok(()) => "done".to_owned(),
            Err(undeleted) => {
                let named = ids.iter().filter(|id| undeleted.why(id).is_some());
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

    fn role(&self, id: &SandboxId) -> Option<&'static str> {
        (self.made.iter())
            .find(|(made, _, _)| made == id)
            .map(|(_, role, _)| *role)
    }
}

fn asked(
    run: &RunTag,
    role: &str,
    class: Name,
    (image, sandboxd): (&ImageRef, &SandboxdRef),
    by: UnixTime,
) -> Create {
    Create {
        run: run.clone(),
        name: Name::try_from(role.to_owned()).unwrap(),
        role: suite_role(role),
        instance: 0,
        class,
        image: image.clone(),
        sandboxd: sandboxd.clone(),
        ops: false,
        model: false,
        // Far past the suite's end, which comes within minutes on any provider.
        hard_end: from_now(Duration::from_hours(1)),
        by,
    }
}

fn reserved(provider: &dyn Provider, run: &RunTag, roles: &[&str]) -> Reservation {
    let phases: Vec<Vec<Role>> = (roles.iter()).map(|&role| vec![suite_role(role)]).collect();
    reserve_with_room(provider, run, &phases)
}

/// What `provider` holds for `run`, of `phases`, once it has room beside what other tests hold,
/// which it waits for for up to 10 minutes.
///
/// # Panics
///
/// Panics if it still has no room then.
pub fn reserve_with_room(
    provider: &dyn Provider,
    run: &RunTag,
    phases: &[Vec<Role>],
) -> Reservation {
    let room = Deadline::after(ROOM);
    loop {
        match provider.reserve(run, phases) {
            Ok(held) => return held,
            Err(error) if room.remaining().is_none() => panic!("{error}"),
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn suite_role(role: &str) -> Role {
    Role {
        name: Name::try_from(role.to_owned()).unwrap(),
        user: serde_json::from_str::<User>(r#"{"uid": 4242, "gid": 4243}"#).unwrap(),
        resources: serde_json::from_str::<Resources>(
            r#"{"cpu_millis": 500, "memory_bytes": 268435456}"#,
        )
        .unwrap(),
        instances: NonZeroU32::MIN,
    }
}

fn from_now(after: Duration) -> UnixTime {
    UnixTime::of(SystemTime::now())
        .unwrap()
        .saturating_add(after)
}

fn sorted(names: impl Iterator<Item = String>) -> Vec<String> {
    let mut names: Vec<String> = names.collect();
    names.sort();
    names
}

impl Drop for Suite<'_> {
    fn drop(&mut self) {
        for run in [&self.run, &self.other] {
            // An error here is printed, never panicked on, since a panic while a failed test
            // unwinds would abort the test binary and hide the other tests' results; only a bug
            // panics.
            let deleted = self.subject.connect().and_then(|provider| {
                let listed = provider.list(run)?;
                provider
                    .delete(run, &ids(listed))
                    .map_err(|error| error.to_string())
            });
            if let Err(error) = deleted {
                eprintln!("run {} can't be cleaned up: {error}", run.as_str());
            }
        }
    }
}

fn ids(listed: Vec<Listed>) -> Vec<SandboxId> {
    (listed.into_iter())
        .map(|Listed { id, ended: _ }| id)
        .collect()
}
