//! Runs' builds on their loop, in a simulated world, with a stand-in at the engine's contract
//! ([`Builder`]) whose builds end built, failed or hung, after a time, as a seed draws them.

use std::num::NonZeroUsize;
use std::time::Duration;

use engine::{Admit, Builder, Builds, Ended, Stepped};
use expect_test::expect;
use stdx::io::{Io, Loop};
use stdx::slots::{Slots, Taker, Taking};
use test_support::TestDir;
use test_support::world::World;

// When a test's runs stop, which a run that waits on a hung build never reaches by itself.
const END: Duration = Duration::from_mins(30);

// SplitMix64, by which TigerBeetle's PRNG seeds itself, and which the JDK's `SplittableRandom`
// draws by:
// https://github.com/tigerbeetle/tigerbeetle/blob/68958eb757747d0fc96a79d1f9c2fa6766b67b48/src/stdx/prng.zig#L125-L132
struct Prng(u64);

impl Prng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    // A number below `bound`, by Lemire's multiplication without its rejection, which skews a
    // draw by at most `bound` in 2^64.
    fn below(&mut self, bound: u64) -> u64 {
        u64::try_from((u128::from(self.next()) * u128::from(bound)) >> 64).unwrap()
    }
}

// What a stand-in's build comes to, built or failed, or none as it hangs, and how long it lasts.
struct Drawn {
    ends: Option<Result<String, String>>,
    lasts: Duration,
}

impl Drawn {
    fn draw(prng: &mut Prng, image: &str) -> Self {
        let lasts = Duration::from_secs(1 + prng.below(600));
        let ends = match prng.below(8) {
            0 => None,
            1 => Some(Err(format!("the recipe of {image} failed"))),
            _ => Some(Ok(image.to_owned())),
        };
        Self { ends, lasts }
    }
}

// A build that the world ends as its draw says, at the time that it draws: never, if it hangs.
struct StandIn {
    world: World,
}

impl Builder for StandIn {
    type Built = String;
    type Missing = Drawn;

    fn found(&self, _: &Drawn) -> Result<Option<String>, String> {
        Ok(None)
    }

    fn build<C: Loop>(
        &self,
        io: &mut Io<C>,
        drawn: Drawn,
        ended: impl FnOnce(Result<String, String>) -> C::Event + Send + 'static,
    ) -> Result<(), String> {
        let Drawn { ends, lasts } = drawn;
        if let Some(built) = ends {
            let events = io.events();
            self.world
                .at(io.now() + lasts, move || events.send(ended(built)));
        }
        Ok(())
    }
}

// A run's admission, which notes what it hears of the slot, and whether its take waits, and fails
// to wait for it if `refuses` says so.
struct Noted {
    lines: Vec<String>,
    refuses: bool,
    waits: bool,
}

impl Admit for Noted {
    fn waiting(&mut self, holders: &[String]) -> Result<(), String> {
        let holders = holders.join(", ");
        self.lines
            .push(format!("waits, as {holders} holds the slot"));
        if self.refuses {
            return Err("its wait can't be marked".to_owned());
        }
        self.waits = true;
        Ok(())
    }

    fn took(&mut self, waited: Option<Duration>) -> Result<(), String> {
        self.waits = false;
        self.lines.push(match waited {
            None => "takes the slot".to_owned(),
            Some(waited) => format!("takes the slot after waiting {}", shown(waited)),
        });
        Ok(())
    }
}

struct Run {
    name: &'static str,
    builds: Builds<StandIn>,
    admission: Noted,
    images: Vec<(String, Drawn)>,
    // How many of its builds have yet to end.
    building: usize,
    arrived: bool,
    released: bool,
}

impl Run {
    // Steps the run's builds, once it has arrived and until it has let go of the slot, and notes
    // what they came to.
    fn step(&mut self, io: &mut Io<Racing>, stand_in: &StandIn, lines: &mut Vec<String>) {
        if !self.arrived || self.released {
            return;
        }
        let at = shown(io.now());
        let name = self.name;
        if !self.images.is_empty() {
            let images: Vec<&str> = self
                .images
                .iter()
                .map(|(image, _)| image.as_str())
                .collect();
            lines.push(format!(
                "at {at}, run {name} arrives to build {}",
                images.join(", ")
            ));
        }
        for (key, (_, drawn)) in self.images.drain(..).enumerate() {
            self.builds.add(key, drawn);
        }
        let Stepped { started, ended } = self.builds.step(io, stand_in, &mut self.admission);
        for noted in self.admission.lines.drain(..) {
            lines.push(format!("at {at}, run {name} {noted}"));
        }
        for key in started {
            lines.push(format!("at {at}, run {name} starts build {key}"));
        }
        for (key, built) in ended {
            lines.push(format!("at {at}, run {name}'s build {key}: {built:?}"));
            self.building -= 1;
        }
        if self.building == 0 {
            self.builds.release(io);
            self.released = true;
            lines.push(format!("at {at}, run {name} ends, with its slot"));
        }
    }
}

// The runs on one loop, each found by its index, which is its token.
struct Racing(Vec<Run>);

impl Loop for Racing {
    type Event = Ended<String>;

    fn event(&mut self, _: &mut Io<Self>, ended: Ended<String>) {
        let token = usize::try_from(ended.token).unwrap();
        self.0[token].builds.ended(ended);
    }
}

impl Taker for Racing {
    fn taking(&mut self, token: u64) -> &mut Taking {
        self.0[usize::try_from(token).unwrap()].builds.taking()
    }
}

fn arrive(racing: &mut Racing, _: &mut Io<Racing>, token: u64) {
    racing.0[usize::try_from(token).unwrap()].arrived = true;
}

fn end(_: &mut Racing, _: &mut Io<Racing>, _: u64) {}

fn shown(time: Duration) -> String {
    format!("{:.1}s", time.as_secs_f64())
}

// Two runs, each of two images that the engine doesn't have, arrive at times that `seed` draws,
// and take one slot as their first build starts. Each lets go of it once each of its builds, which
// end as the seed draws, has ended. The run `refusing` names fails to wait for the slot.
fn raced(seed: u64, refusing: Option<&str>) -> Vec<String> {
    let dir = TestDir::new();
    let world = World::new();
    let mut prng = Prng(seed);
    let slots = Slots {
        dir: dir.path().join("slots"),
        count: NonZeroUsize::MIN,
    };
    let mut racing = Racing(Vec::new());
    // Each run's take and arrival, and the end.
    let mut io = Io::new(world.backend(), 5);
    for (token, name) in (0..).zip(["a", "b"]) {
        let images = (0..2).map(|index| {
            let image = format!("{name}{index}");
            let drawn = Drawn::draw(&mut prng, &image);
            (image, drawn)
        });
        racing.0.push(Run {
            name,
            builds: Builds::new(token, Some((slots.clone(), format!("run {name}")))),
            admission: Noted {
                lines: Vec::new(),
                refuses: refusing == Some(name),
                waits: false,
            },
            images: images.collect(),
            building: 2,
            arrived: false,
            released: false,
        });
        let arrives = Duration::from_millis(prng.below(2_000));
        io.timeout(arrives, token, arrive);
    }
    io.timeout(END, 0, end);
    let stand_in = StandIn {
        world: world.clone(),
    };
    let mut lines = Vec::new();
    while io.now() < END {
        io.run_until(&mut racing, Duration::MAX);
        for run in &mut racing.0 {
            run.step(&mut io, &stand_in, &mut lines);
        }
    }
    for run in racing.0.iter().filter(|run| !run.released) {
        lines.push(format!("at {}, run {} still builds", shown(END), run.name));
    }
    // A take that still waits has a look in flight, which ends as it takes the slot that each run
    // lets go of.
    racing
        .0
        .iter_mut()
        .for_each(|run| run.builds.release(&mut io));
    while racing.0.iter().any(|run| run.admission.waits) {
        io.run_until(&mut racing, Duration::MAX);
        for run in &mut racing.0 {
            run.builds.step(&mut io, &stand_in, &mut run.admission);
        }
    }
    lines
}

#[test]
fn racing_for_a_slot() {
    expect![[r#"
        at 0.8s, run b arrives to build b0, b1
        at 0.8s, run b takes the slot
        at 0.8s, run b starts build 0
        at 0.8s, run b starts build 1
        at 0.9s, run a arrives to build a0, a1
        at 0.9s, run a waits, as run b holds the slot
        at 150.8s, run b's build 0: Ok("b0")
        at 197.8s, run b's build 1: Err("the recipe of b1 failed")
        at 197.8s, run b ends, with its slot
        at 197.9s, run a takes the slot after waiting 197.0s
        at 197.9s, run a starts build 0
        at 197.9s, run a starts build 1
        at 738.9s, run a's build 1: Ok("a1")
        at 1800.0s, run a still builds"#]]
    .assert_eq(&raced(7, None).join("\n"));
}

#[test]
fn refused_its_wait() {
    expect![[r#"
        at 0.8s, run b arrives to build b0, b1
        at 0.8s, run b takes the slot
        at 0.8s, run b starts build 0
        at 0.8s, run b starts build 1
        at 0.9s, run a arrives to build a0, a1
        at 0.9s, run a waits, as run b holds the slot
        at 0.9s, run a's build 0: Err("its wait can't be marked")
        at 0.9s, run a's build 1: Err("its wait can't be marked")
        at 0.9s, run a ends, with its slot
        at 150.8s, run b's build 0: Ok("b0")
        at 197.8s, run b's build 1: Err("the recipe of b1 failed")
        at 197.8s, run b ends, with its slot"#]]
    .assert_eq(&raced(7, Some("a")).join("\n"));
}
