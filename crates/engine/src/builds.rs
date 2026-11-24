//! A loop's builds of images that [`Engine::look`] didn't find ([`Builds`]).
//!
//! Each build waits for a slot, if the builds take one, which the loop takes, then for its
//! admission, then runs on a [`Builder`], which completes it into the loop as an event: or never,
//! as a build may hang. What a build needs is owned, so the engine's build runs on an edge worker
//! of its own, which waits on `docker build`.

use std::time::Duration;
use std::{mem, thread};

use stdx::io::{Io, Loop};
use stdx::slots::{Slots, Taker, Taking, Told};

use crate::{Built, Engine, Missing};

/// What a loop's builds run on: the engine, or a stand-in in a test.
pub trait Builder {
    /// What a build gives.
    type Built: Send + 'static;
    /// What a build needs.
    type Missing;

    /// The image that another build of the same inputs made since the look that gave `missing`,
    /// if any, as the build starts.
    fn found(&self, missing: &Self::Missing) -> Result<Option<Self::Built>, String>;

    /// Starts building `missing`, then sends `io`'s loop the event that `ended` makes of what the
    /// build came to, unless the build hangs. It returns at once. Fails if the build can't start,
    /// and then never calls `ended`.
    fn build<C: Loop>(
        &self,
        io: &mut Io<C>,
        missing: Self::Missing,
        ended: impl FnOnce(Result<Self::Built, String>) -> C::Event + Send + 'static,
    ) -> Result<(), String>;
}

// On an edge worker, which waits for `docker build` to end.
impl Builder for Engine {
    type Built = Built;
    type Missing = Missing;

    fn found(&self, missing: &Missing) -> Result<Option<Built>, String> {
        missing.found()
    }

    fn build<C: Loop>(
        &self,
        io: &mut Io<C>,
        missing: Missing,
        ended: impl FnOnce(Result<Built, String>) -> C::Event + Send + 'static,
    ) -> Result<(), String> {
        let edge = io.edge();
        let spawned = (thread::Builder::new().name("build".to_owned()))
            .spawn(move || edge.send(ended(missing.build())));
        spawned
            .map(drop)
            .map_err(|error| format!("starting the build's thread failed: {error}"))
    }
}

/// What a loop's builds ask of their caller: to hold each image, to hear of the slot that they
/// wait for, and to admit each build.
///
/// By default it holds nothing, hears nothing and admits every build.
pub trait Admit {
    /// Holds the image tagged `tag`, from now until the caller lets it go, as [`Engine::look`]
    /// asks before it looks the image up; an error fails the build.
    fn hold(&mut self, _tag: &str) -> Result<(), String> {
        Ok(())
    }

    /// Hears that each slot is held, by what each holder named, as the builds start to wait; an
    /// error fails each build that waits.
    fn waiting(&mut self, _holders: &[String]) -> Result<(), String> {
        Ok(())
    }

    /// Hears that the builds hold a slot, after waiting this long, if they waited; an error lets
    /// it go, and fails each build that waits.
    fn took(&mut self, _waited: Option<Duration>) -> Result<(), String> {
        Ok(())
    }

    /// Admits a build as it starts; an error refuses it, and fails the build.
    fn admit(&mut self) -> Result<(), String> {
        Ok(())
    }
}

impl Admit for () {}

/// The event of a build's end, which the loop's owner hands the [`Builds`] made with `token`.
#[derive(Debug)]
pub struct Ended<T> {
    pub token: u64,
    /// The key that the build was added with ([`Builds::add`]).
    pub key: usize,
    pub built: Result<T, String>,
}

/// What the builds' [`step`](Builds::step) started and what ended, each by its key.
#[derive(Debug)]
pub struct Stepped<T> {
    pub started: Vec<usize>,
    pub ended: Vec<(usize, Result<T, String>)>,
}

/// A loop's builds: each waits for a slot, if they take one, and for its admission, then runs on a
/// [`Builder`] ([`Builds::step`]).
///
/// The builds share one slot, which they take as the first waits, and hold until
/// [`Builds::release`]. Their operations carry their token.
pub struct Builds<B: Builder> {
    token: u64,
    // The take of the builds' slot, if they take one, and whether it has started.
    taking: Option<Taking>,
    asked: bool,
    // Why each build that waits fails, once the slot can't be held.
    refused: Option<String>,
    waiting: Vec<(usize, B::Missing)>,
    running: usize,
    ended: Vec<(usize, Result<B::Built, String>)>,
}

impl<B: Builder> Builds<B> {
    /// Builds whose operations carry `token`, and which take one of `slots`, named for its holder
    /// in it, if given.
    pub fn new(token: u64, slots: Option<(Slots, String)>) -> Self {
        Self {
            token,
            taking: slots.map(|(slots, holder)| Taking::new(slots, holder)),
            asked: false,
            refused: None,
            waiting: Vec::new(),
            running: 0,
            ended: Vec::new(),
        }
    }

    /// Adds the build of `key`, which `missing` says, to start at the next step that may.
    pub fn add(&mut self, key: usize, missing: B::Missing) {
        self.waiting.push((key, missing));
    }

    /// Takes the event of a build's end.
    ///
    /// # Panics
    ///
    /// Panics if the event is of other builds', or of none that runs.
    pub fn ended(&mut self, ended: Ended<B::Built>) {
        let Ended { token, key, built } = ended;
        assert!(
            token == self.token && self.running > 0,
            "the end of build {key} of builds {token} reached builds {}, of which {} run: \
             expected the end of one of theirs that runs",
            self.token,
            self.running
        );
        self.running -= 1;
        self.ended.push((key, built));
    }

    /// The take of the builds' slot, for the loop's owner to find ([`Taker::taking`]).
    ///
    /// # Panics
    ///
    /// Panics if the builds take no slot.
    pub fn taking(&mut self) -> &mut Taking {
        let Some(taking) = &mut self.taking else {
            unreachable!(
                "a slot's take reached builds that take none, though only a take submits its \
                 operations"
            )
        };
        taking
    }

    /// Goes on, after `add` and after each of the loop's batches: tells `admission` of what came
    /// of the slot, then starts on `builder` each build that `admission` admits once the slot is
    /// held, unless `builder` finds its image by then, and gives what started and what ended since
    /// the step before.
    pub fn step<C>(
        &mut self,
        io: &mut Io<C>,
        builder: &B,
        admission: &mut dyn Admit,
    ) -> Stepped<B::Built>
    where
        C: Taker + Loop<Event = Ended<B::Built>>,
    {
        let token = self.token;
        let mut ended = mem::take(&mut self.ended);
        if let Some(taking) = &mut self.taking {
            if !self.asked && !self.waiting.is_empty() && self.refused.is_none() {
                self.asked = true;
                taking.take(io, token);
            }
            for told in taking.told() {
                let held = match told {
                    Told::Waiting(holders) => {
                        (admission.waiting(&holders)).map(|()| taking.wait(io, token))
                    }
                    Told::Took(waited) => (admission.took(waited)).inspect_err(|_| {
                        taking.release(io);
                    }),
                    Told::Failed(why) => Err(why),
                };
                if let Err(why) = held {
                    self.refused = Some(why);
                }
            }
        }
        let mut started = Vec::new();
        if let Some(why) = &self.refused {
            let failed = self
                .waiting
                .drain(..)
                .map(|(key, _)| (key, Err(why.clone())));
            ended.extend(failed);
        } else if self.taking.as_ref().is_none_or(Taking::held) {
            for (key, missing) in self.waiting.drain(..) {
                let found = admission.admit().and_then(|()| builder.found(&missing));
                let built = match found {
                    Ok(None) => {
                        builder.build(io, missing, move |built| Ended { token, key, built })
                    }
                    Ok(Some(built)) => {
                        ended.push((key, Ok(built)));
                        continue;
                    }
                    Err(why) => Err(why),
                };
                match built {
                    Ok(()) => {
                        self.running += 1;
                        started.push(key);
                    }
                    Err(why) => ended.push((key, Err(why))),
                }
            }
        }
        Stepped { started, ended }
    }

    /// Lets go of the slot, if the builds hold one.
    pub fn release<C: Loop>(&mut self, io: &mut Io<C>) {
        if let Some(taking) = &mut self.taking {
            taking.release(io);
        }
    }
}
