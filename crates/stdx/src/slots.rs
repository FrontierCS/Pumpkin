//! Slots that processes take, at most their count at once, which a loop takes ([`Taking`]).
//!
//! A slot is held as a counting semaphore's is, but by a lock on a file, which the kernel lets
//! go however its holder ends. No lock tells a taker that it was let go, so a taker that finds
//! each held looks again, at a time on its loop's clock.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;
use std::{fs, io, mem};

use crate::Context as _;
use crate::io::{File, Io, Loop};

// How long a taker waits, once each slot is held, before it looks again. What a slot is held for
// lasts seconds or minutes.
const POLL: Duration = Duration::from_millis(500);

/// `count` slots, the files `0` to `count - 1` of `dir`, which a take makes if need be.
#[derive(Clone, Debug)]
pub struct Slots {
    pub dir: PathBuf,
    pub count: NonZeroUsize,
}

/// The owner of a loop that takes slots, which finds each [`Taking`] by the token of its take.
pub trait Taker: Loop {
    fn taking(&mut self, token: u64) -> &mut Taking;
}

/// What a take tells its owner, in order ([`Taking::told`]).
#[derive(Debug)]
pub enum Told {
    /// Each slot is held, by what each holder named: the take waits until [`Taking::wait`].
    Waiting(Vec<String>),
    /// It holds a slot, after waiting this long, if it waited.
    Took(Option<Duration>),
    Failed(String),
}

/// A take of one of [`Slots`] on a loop, then the slot it holds, until [`Taking::release`].
///
/// It takes the first slot that none holds, or, while each is held, the first let go, looked for
/// every `POLL`, and names its holder in it.
pub struct Taking {
    slots: Slots,
    holder: String,
    // The slot that the take tries, with its file on the loop.
    trying: Option<(usize, File)>,
    // What the holders of the slots tried so far named.
    holders: Vec<String>,
    held: Option<File>,
    // When the take first found each slot held.
    since: Option<Duration>,
    told: Vec<Told>,
}

impl Taking {
    pub fn new(slots: Slots, holder: String) -> Self {
        Self {
            slots,
            holder,
            trying: None,
            holders: Vec::new(),
            held: None,
            since: None,
            told: Vec::new(),
        }
    }

    /// Starts the take, whose operations carry `token`.
    pub fn take<C: Taker>(&mut self, io: &mut Io<C>, token: u64) {
        let dir = &self.slots.dir;
        match fs::create_dir_all(dir).context(dir.display()) {
            Ok(()) => self.try_slot(io, token, 0),
            Err(error) => self.told.push(Told::Failed(error)),
        }
    }

    /// Goes on with a take that told [`Told::Waiting`]: it looks again every `POLL`.
    pub fn wait<C: Taker>(&mut self, io: &mut Io<C>, token: u64) {
        io.timeout(io.now() + POLL, token, look_again::<C>);
    }

    /// What the take has told since it was last asked.
    pub fn told(&mut self) -> Vec<Told> {
        mem::take(&mut self.told)
    }

    pub fn held(&self) -> bool {
        self.held.is_some()
    }

    /// Lets go of the slot that it holds, if any.
    pub fn release<C: Loop>(&mut self, io: &mut Io<C>) {
        if let Some(held) = self.held.take() {
            io.close_file(held);
        }
    }

    fn try_slot<C: Taker>(&mut self, io: &mut Io<C>, token: u64, index: usize) {
        let path = self.slots.dir.join(index.to_string());
        let opened = (fs::OpenOptions::new().read(true).write(true).create(true))
            .truncate(false)
            .open(&path);
        match opened.context(path.display()) {
            Ok(opened) => {
                let file = io.file(opened);
                io.try_lock(&file, token, locked::<C>);
                self.trying = Some((index, file));
            }
            Err(error) => self.told.push(Told::Failed(error)),
        }
    }

    fn locked<C: Taker>(&mut self, io: &mut Io<C>, token: u64, locked: io::Result<bool>) {
        let Some((index, file)) = self.trying.take() else {
            unreachable!("a slot's lock completed with none tried, though a take tries one at once")
        };
        let path = self.slots.dir.join(index.to_string());
        let named = match locked {
            Ok(true) => {
                let named = fs::write(&path, &self.holder).context(path.display());
                match named {
                    Ok(()) => {
                        self.held = Some(file);
                        let waited = self.since.map(|since| io.now().saturating_sub(since));
                        self.told.push(Told::Took(waited));
                    }
                    Err(error) => {
                        io.close_file(file);
                        self.told.push(Told::Failed(error));
                    }
                }
                return;
            }
            Ok(false) => fs::read_to_string(&path),
            Err(error) => Err(error),
        };
        io.close_file(file);
        match named.context(path.display()) {
            // A holder that has yet to write its name is named by its slot's.
            Ok(named) if named.is_empty() => self.holders.push(index.to_string()),
            Ok(named) => self.holders.push(named),
            Err(error) => return self.told.push(Told::Failed(error)),
        }
        if index + 1 < self.slots.count.get() {
            return self.try_slot(io, token, index + 1);
        }
        let holders = mem::take(&mut self.holders);
        if self.since.is_some() {
            return self.wait(io, token);
        }
        self.since = Some(io.now());
        self.told.push(Told::Waiting(holders));
    }
}

fn locked<C: Taker>(owner: &mut C, io: &mut Io<C>, token: u64, locked: io::Result<bool>) {
    owner.taking(token).locked(io, token, locked);
}

fn look_again<C: Taker>(owner: &mut C, io: &mut Io<C>, token: u64) {
    owner.taking(token).try_slot(io, token, 0);
}

/// What `call` gives, called while it holds one of `slots`, which it names `holder` in: for a
/// caller that may block, as on an edge worker, taken on a loop of its own on the machine.
///
/// While each slot is held, `waiting` hears what each holder named, once, before the wait.
///
/// # Errors
///
/// Fails as `call` does, or if no loop can be made, or a slot's file can't be made, locked or
/// named.
pub fn holding<T>(
    slots: Slots,
    holder: &str,
    waiting: impl FnOnce(&[String]),
    call: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    struct Alone(Taking);

    impl Loop for Alone {
        type Event = ();

        fn event(&mut self, _: &mut Io<Self>, (): ()) {}
    }

    impl Taker for Alone {
        fn taking(&mut self, _: u64) -> &mut Taking {
            &mut self.0
        }
    }

    let machine = crate::io::machine().context("making a loop to take a slot on failed")?;
    // One operation at a time: a slot's lock, or the wait before the next look.
    let mut io = Io::new(machine, 1);
    let mut alone = Alone(Taking::new(slots, holder.to_owned()));
    alone.0.take(&mut io, 0);
    let mut waiting = Some(waiting);
    loop {
        for told in alone.0.told() {
            match told {
                Told::Waiting(holders) => {
                    if let Some(waiting) = waiting.take() {
                        waiting(&holders);
                    }
                    alone.0.wait(&mut io, 0);
                }
                Told::Took(_) => {
                    let called = call();
                    alone.0.release(&mut io);
                    return called;
                }
                Told::Failed(error) => return Err(error),
            }
        }
        io.run_until(&mut alone, Duration::MAX);
    }
}
