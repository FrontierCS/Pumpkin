//! The conformance suite every [`Provider`] passes, through the trait alone: one [`check`],
//! whose outcomes are the same for every provider, run against [`Sim`] and against Docker,
//! whose test is ignored, as the Docker provider's are ([`docker`](crate::docker)). What every
//! provider shares is tested here; a provider's own tests cover only what its platform adds,
//! and the tests of `sandboxd` that need a real sandbox, which run on Docker.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]
#![expect(
    clippy::print_stderr,
    reason = "a run that can't be cleaned up says so"
)]

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use expect_test::expect;
use pumpkin_gateway::{Listener, Put, Session};
use pumpkin_provider::Provider;
use pumpkin_schema::ir::{AbsolutePath, Image, Name, Resources, User};
use pumpkin_schema::provider::{
    Console, Create, Created, Dial, Listed, Local, Published, Role, RunTag, SOCKET_NAME, SandboxId,
};
use pumpkin_schema::sandbox::{RelativePath, file_mode};
use stdx::{Context, Deadline};

use crate::docker;
use crate::sim::Sim;

/// A provider under the suite, and how its platform makes a sandbox's `sandboxd` misbehave.
pub(crate) trait Subject: Provider {
    /// What the suite publishes, which may need a file in `dir`, a directory the suite removes.
    fn locals(&self, dir: &Path) -> Locals;

    /// Makes sandbox `id`'s `sandboxd` stop reading its session before it answers another
    /// request.
    fn stall(&self, id: &SandboxId);
}

/// What the suite publishes, which only a subject's platform can name.
pub(crate) struct Locals {
    /// The role's image, which every sandbox of the suite runs.
    pub(crate) image: Image,
    /// A `sandboxd` that dials.
    pub(crate) sandboxd: PathBuf,
    /// A `sandboxd` that starts but never dials.
    pub(crate) mute: PathBuf,
}

#[test]
fn sim() {
    check("sim", &Sim::new());
}

#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn docker() {
    check("docker", docker::engine());
}

/// A wait that nothing can end sooner, such as an accept of a `sandboxd` that never dials, no
/// longer than any provider's startup, so that no outcome rests on its length.
fn soon() -> Deadline {
    Deadline::after(Duration::from_secs(1))
}

/// Runs the suite against `subject`, as test `name`, and checks its outcomes, which every
/// provider shares.
fn check(name: &str, subject: &dyn Subject) {
    let provider: &dyn Provider = subject;
    let mut suite = Suite::new(name, provider);
    let Locals {
        image,
        sandboxd,
        mute,
    } = subject.locals(&suite.dir);
    let (sandboxd, mute) = (
        Local::Sandboxd { path: sandboxd },
        Local::Sandboxd { path: mute },
    );
    let published = provider.publish(&[Local::Role { image }, sandboxd, mute]);
    let [image, sandboxd, mute] = <[Published; 3]>::try_from(published.unwrap()).unwrap();
    let mut lines = lifecycle(&mut suite, &image, &sandboxd);
    lines.extend(never_dials(&mut suite, &image, &mute));
    lines.extend(stalls(&mut suite, subject, &image, &sandboxd));
    expect![[r#"
        lifecycle: accepted and set up by its startup
        listed: ["lifecycle"]
        listed as another run's: []
        deleted as another run's: unknown ["lifecycle"]
        its console, as another run's: refused
        listed: ["lifecycle"]
        deleted: unknown []
        listed: []
        again, and an id no sandbox has: deleted: unknown ["lifecycle", "pumpkin-none"]
        mute: accepted: Err(TimedOut)
        its console: read
        deleted: unknown []
        stalled: accepted and set up by its startup
        then a put: TimedOut: the sandbox didn't take what was sent before the request's deadline: expected it to read while the deadline lasts
        deleted: unknown []
        listed: []"#]].assert_eq(&lines.join("\n"));
}

/// A sandbox started, listed and deleted, which another run can neither list, delete nor read;
/// deleting it again returns its id, as deleting an id no sandbox has does.
fn lifecycle(suite: &mut Suite<'_>, image: &Published, sandboxd: &Published) -> Vec<String> {
    let provider = suite.provider;
    let other = RunTag::try_from(format!("{}-other", suite.run.as_str())).unwrap();
    let none = SandboxId::from("pumpkin-none".to_owned());
    let (id, session) = suite.start("lifecycle", image, sandboxd);
    // Each line makes its calls as it is computed, so the lines' order is the calls'.
    vec![
        format!("lifecycle: {}", outcome(&session)),
        suite.listed(),
        format!(
            "listed as another run's: {:?}",
            suite.named(ids(provider.list(&other).unwrap()))
        ),
        format!(
            "deleted as another run's: unknown {:?}",
            suite.named(provider.delete(&other, std::slice::from_ref(&id)).unwrap())
        ),
        format!(
            "its console, as another run's: {}",
            read(&provider.console(&other, &id))
        ),
        suite.listed(),
        suite.delete(std::slice::from_ref(&id)),
        suite.listed(),
        format!(
            "again, and an id no sandbox has: {}",
            suite.delete(&[id, none])
        ),
    ]
}

/// A sandbox whose `sandboxd` never dials: its accept times out, and it is read and deleted.
fn never_dials(suite: &mut Suite<'_>, image: &Published, mute: &Published) -> Vec<String> {
    let (id, listener) = suite.create("mute", image, mute);
    let (arrivals, _) = mpsc::channel();
    let accepted = listener.accept(soon(), None, false, arrivals, |arrival| arrival);
    let accepted = accepted.map(|_| ());
    vec![
        format!(
            "mute: accepted: {:?}",
            accepted.map_err(|error| error.kind())
        ),
        format!(
            "its console: {}",
            read(&suite.provider.console(&suite.run, &id))
        ),
        suite.delete(&[id]),
    ]
}

/// A sandbox whose `sandboxd` stalls: a request then times out, and it is deleted.
fn stalls(
    suite: &mut Suite<'_>,
    subject: &dyn Subject,
    image: &Published,
    sandboxd: &Published,
) -> Vec<String> {
    let (id, session) = suite.start("stalled", image, sandboxd);
    let started = format!("stalled: {}", outcome(&session));
    subject.stall(&id);
    vec![
        started,
        format!("then a put: {}", put(session.unwrap())),
        suite.delete(&[id]),
        suite.listed(),
    ]
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
fn outcome(session: &Result<Session, String>) -> String {
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

/// The suite's run on one provider, and a directory, private to this user, for the sockets its
/// sandboxes dial. Dropping it deletes what the run's list finds, and removes the directory.
struct Suite<'a> {
    provider: &'a dyn Provider,
    dir: PathBuf,
    run: RunTag,
    /// The id of each sandbox created, and its role, by which the outcomes name it.
    roles: Vec<(SandboxId, &'static str)>,
}

impl<'a> Suite<'a> {
    fn new(name: &str, provider: &'a dyn Provider) -> Self {
        // Socket paths are short: a directory in the system's temporary one, as the parent of
        // every socket directory must be private to this user.
        let tag = format!("pk-{}-{name}", std::process::id());
        let dir = std::env::temp_dir().join(&tag);
        let _ = fs::remove_dir_all(&dir);
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        Self {
            provider,
            dir,
            run: RunTag::try_from(tag).unwrap(),
            roles: Vec::new(),
        }
    }

    /// Creates a sandbox of the run for `role`, which runs `image` and `sandboxd`, and returns
    /// its id and the listener where it dials.
    fn create(
        &mut self,
        role: &'static str,
        image: &Published,
        sandboxd: &Published,
    ) -> (SandboxId, Listener) {
        let dir = self.dir.join(role);
        fs::create_dir(&dir).unwrap();
        let listener = Listener::bind(dir.join(SOCKET_NAME)).unwrap();
        let create = Create {
            run: self.run.clone(),
            role: Role {
                name: Name::try_from(role.to_owned()).unwrap(),
                user: serde_json::from_str::<User>(r#"{"uid": 4242, "gid": 4243}"#).unwrap(),
                resources: serde_json::from_str::<Resources>(
                    r#"{"cpu_millis": 500, "memory_bytes": 268435456}"#,
                )
                .unwrap(),
            },
            image: image.clone(),
            sandboxd: sandboxd.clone(),
            ops: false,
            dial: Dial::Socket { dir },
        };
        let Created { id } = self.provider.create(&create).unwrap();
        self.roles.push((id.clone(), role));
        (id, listener)
    }

    /// Creates a sandbox for `role`, as [`create`](Self::create) does, and accepts its session,
    /// set up, by the startup the provider declares, from the create's start.
    fn start(
        &mut self,
        role: &'static str,
        image: &Published,
        sandboxd: &Published,
    ) -> (SandboxId, Result<Session, String>) {
        let startup = self.provider.ready().capabilities.startup_seconds;
        let by = Deadline::after(Duration::from_secs(startup.get()));
        let (id, listener) = self.create(role, image, sandboxd);
        // The suite starts no commands, so nothing arrives that it needs.
        let (arrivals, _) = mpsc::channel();
        let accepted = listener.accept(by, None, false, arrivals, |arrival| arrival);
        let accepted = accepted.map_err(|error| error.to_string());
        let session = accepted.and_then(|mut session| {
            session
                .set_up(by)
                .context(format_args!("sandbox {}", id.as_str()))?;
            Ok(session)
        });
        (id, session)
    }

    /// What deleting `ids` from the run returns.
    fn delete(&self, ids: &[SandboxId]) -> String {
        let unknown = self.provider.delete(&self.run, ids);
        format!("deleted: unknown {:?}", self.named(unknown.unwrap()))
    }

    /// What the run's list finds.
    fn listed(&self) -> String {
        let listed = self.provider.list(&self.run).unwrap();
        format!("listed: {:?}", self.named(ids(listed)))
    }

    /// Each id as the role of the sandbox it names, if the suite created one, sorted.
    fn named(&self, ids: Vec<SandboxId>) -> Vec<String> {
        let role = |id: SandboxId| match self.roles.iter().find(|(created, _)| *created == id) {
            Some((_, role)) => (*role).to_owned(),
            None => id.as_str().to_owned(),
        };
        let mut named: Vec<String> = ids.into_iter().map(role).collect();
        named.sort();
        named
    }
}

impl Drop for Suite<'_> {
    // An error here is printed, never panicked on, since a panic while a failed test unwinds
    // would abort the test binary and hide the other tests' results; only a bug panics.
    fn drop(&mut self) {
        let listed = self.provider.list(&self.run);
        let deleted = listed.and_then(|listed| self.provider.delete(&self.run, &ids(listed)));
        if let Err(error) = deleted {
            eprintln!("run {} can't be cleaned up: {error}", self.run.as_str());
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The ids of the sandboxes a list found.
fn ids(listed: Vec<Listed>) -> Vec<SandboxId> {
    listed.into_iter().map(|Listed { id }| id).collect()
}
