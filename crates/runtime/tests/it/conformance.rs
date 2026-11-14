//! The simulated provider ([`Sim`]) against the suite every provider passes
//! ([`test_support::suite`]): as it is, as one whose sandboxes listen, and as a provider that
//! frees an ended sandbox itself, whose outcomes are the same, as `Sweep::list` allows it.

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime};

use expect_test::expect;
use gateway::Dialer;
use provider::{Create, Creator, Provider, Role, RunTag, SandboxId, Sweep as _, UnixTime};
use schema::ir::{Name, Resources, User};
use stdx::Deadline;
use test_support::suite::{self, Locals, Subject};

use crate::sim::{self, SLOW, Sim, VANISHING};

#[test]
fn suite() {
    suite::check(&[
        ("sim", &Sim::new()),
        ("listening", &Sim::new().listening()),
        ("freeing", &Freeing(Sim::new())),
    ]);
}

/// The simulated provider, whose mute `sandboxd` is [`VANISHING`]'s, so that it frees the
/// suite's ended sandbox itself.
struct Freeing(Sim);

impl Subject for Freeing {
    fn connect(&self) -> Result<Box<dyn Provider + '_>, String> {
        self.0.connect()
    }

    fn locals(&self, dir: &Path) -> Locals {
        Locals {
            mute: PathBuf::from(VANISHING),
            ..self.0.locals(dir)
        }
    }

    fn stall(&self, id: &SandboxId) {
        self.0.stall(id);
    }

    fn held(&self, run: &RunTag) -> Vec<SandboxId> {
        self.0.held(run)
    }

    fn reserves(&self) -> bool {
        self.0.reserves()
    }
}

// A create that the platform finishes after its deadline, as one may finish a create that its
// client gave up on, leaves a sandbox that the run's list finds; where it listens for the
// gateway, which no start claims it for any more, it ends itself, so the list finds it ended.
#[test]
fn late_create() {
    let sim = Sim::new().listening();
    let run = RunTag::try_from(format!("pk-{}-late", std::process::id())).unwrap();
    let role = || Role {
        name: Name::try_from("judge".to_owned()).unwrap(),
        user: serde_json::from_str::<User>(r#"{"uid": 4242, "gid": 4243}"#).unwrap(),
        resources: serde_json::from_str::<Resources>(
            r#"{"cpu_millis": 500, "memory_bytes": 268435456}"#,
        )
        .unwrap(),
        instances: NonZeroU32::MIN,
    };
    let _held = sim.reserve(&run, &[vec![role()]]).unwrap();
    let (dial, _unreached) = Dialer::new(test_support::edge::client()).issue().unwrap();
    let now = UnixTime::of(SystemTime::now()).unwrap();
    let create = Create {
        run: run.clone(),
        name: Name::try_from("job-0".to_owned()).unwrap(),
        role: role(),
        instance: 0,
        class: Name::try_from("sim".to_owned()).unwrap(),
        image: sim.publish_image(&sim::image(SLOW)).unwrap(),
        sandboxd: sim.publish_sandboxd(Path::new("/sim/sandboxd")).unwrap(),
        ops: false,
        model: false,
        hard_end: now.saturating_add(Duration::from_hours(1)),
        by: now.saturating_add(Duration::from_millis(500)),
    };
    let created = sim.create(&create, dial).map(|(created, _)| created.id);
    let listed = |run: &RunTag| sim.list(run).unwrap();
    let left = listed(&run);
    let waited = Deadline::after(Duration::from_secs(10));
    while listed(&run).iter().any(|listed| !listed.ended) && waited.remaining().is_some() {
        thread::sleep(Duration::from_millis(10));
    }
    let ended = listed(&run)
        .iter()
        .map(|listed| listed.ended)
        .collect::<Vec<_>>();
    let console = sim.console(&run, &SandboxId::from("sim-1".to_owned()));
    let ids: Vec<SandboxId> = left.into_iter().map(|listed| listed.id).collect();
    sim.delete(&run, &ids).unwrap();
    let lines = [
        format!("the create: {created:?}"),
        format!("what it left, ended by itself: {ended:?}"),
        format!(
            "why, as its console ends: {}",
            console.unwrap().text.rsplit(": ").next().unwrap()
        ),
    ];
    expect![[r#"
        the create: Err("the simulated create gave up at its deadline, and can't name what it made: expected the run's list to find it")
        what it left, ended by itself: [true]
        why, as its console ends: no gateway claimed the sandbox by its deadline"#]].assert_eq(&lines.join("\n"));
}
