//! A simulated world for a process's loop ([`stdx::io::Backend`]), one loop at a time.
//!
//! The world holds a clock, Unix sockets as streams in memory, and the paths of real files. A sync
//! of a file makes nothing last and completes as the world's script says. A file's lock is the
//! world's, which a close lets go.
//!
//! The world moves its clock by itself only to complete a sync that the script delays, a timeout,
//! or a stand-in's call that it times ([`World::at`]). On the way it stops at the time that the
//! loop waits until. It never jumps to the loop's deadline by itself, as other threads may still
//! send the loop what it waits for. Other threads move the clock too ([`World::reach`],
//! [`Peer::move_on_write`]). A wait with nothing to complete and nothing sent blocks for real, a
//! few milliseconds at a time.
// The world simulates the loop's operations, below TigerBeetle's simulator, which stands in for
// the storage and the network above its IO:
// https://github.com/tigerbeetle/tigerbeetle/blob/68958eb757747d0fc96a79d1f9c2fa6766b67b48/src/testing/cluster.zig#L65-L74

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use stdx::io::{Backend, Durable, Handle, Op, Outcome, Slot};

// How many bytes a stream holds that its reader hasn't taken, each way, past which a send waits:
// Linux's default for a socket's send buffer, `net.core.wmem_default`.
const STREAM_BYTES: usize = 212_992;

// How long an idle wait blocks for real before it returns, so that its loop looks for a signal.
const IDLE: Duration = Duration::from_millis(10);

// Told of each sync, by its file's path and how much it makes last.
type Observer = Box<dyn Fn(&Path, Durable) + Send>;

/// A world, which each of its clones shares: the clock, the sockets and the script.
#[derive(Clone)]
pub struct World {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

/// What a sync of a file meets, which the first script that its path matches says.
pub enum Synced {
    /// It fails at once.
    Fails,
    /// It completes at `at` on the world's clock, once `events` events have reached the loop since
    /// its sync before it began, or its backend was made, as a disk may hold a sync while others
    /// hand the loop what they saw.
    ///
    /// Each wake of the loop ([`Backend::waker`]) is an event, an [`Edge`](stdx::io::Edge)
    /// dropped unsent's too. So is each `write` call of a peer's that sends bytes, and a peer's
    /// end, once, at its shutdown or its last handle's drop. These count on a stream that the loop
    /// holds, or else as the loop accepts it: never on one that it has shut, closed or never
    /// accepts.
    Lasts { at: Duration, events: usize },
    /// It completes once its function says so of the file's path, which the world asks as it
    /// waits. The function runs under the world's lock, so it must not call the world.
    Held(Box<dyn Fn(&Path) -> bool + Send>),
}

/// A sync's script: what a sync of a file meets, if `matches` says it is the file's, holding what
/// it holds. Its functions run under the world's lock, so they must not call the world.
pub struct Script {
    pub matches: Box<dyn Fn(&Path) -> bool + Send>,
    pub synced: Synced,
}

#[derive(Default)]
struct State {
    now: Duration,
    unix: Option<SystemTime>,
    pipes: Vec<Pipe>,
    scripts: VecDeque<Script>,
    // What is told of each sync, by the path of its file and how much it makes last.
    observers: Vec<Observer>,
    // The events that have reached the loop, as `Synced::Lasts` counts them.
    events: usize,
    // Whether a loop's backend lives, as a world has one at a time. The fields below are that
    // loop's.
    looped: bool,
    objects: Vec<(u32, Option<Object>)>,
    free: Vec<u32>,
    listeners: BTreeMap<PathBuf, u32>,
    parked: Vec<(Slot, Op)>,
    ready: Vec<(Slot, Outcome)>,
    // Syncs that a script holds, with when each began, by the count of events sent, and the path
    // of its file.
    held: Vec<(Slot, Synced, usize, PathBuf)>,
    woken: bool,
    // How many things had reached the loop as the last sync began.
    synced_events: usize,
    // Each file's lock, by its path, with the handle that holds it.
    locks: BTreeMap<PathBuf, Handle>,
    // What the world calls once its clock reaches each time ([`World::at`]).
    timed: Vec<(Duration, Timed)>,
}

// A call that a stand-in times on the world's clock.
type Timed = Box<dyn FnOnce() + Send>;

enum Object {
    Listener(PathBuf),
    Stream(usize),
    // A file's path alone, as the world syncs nothing for real.
    File(PathBuf),
}

struct Pipe {
    // What the peer sent the loop, and what the loop sent the peer.
    to_loop: VecDeque<u8>,
    to_peer: VecDeque<u8>,
    end: End,
    peer_shut: bool,
    // The peer reads no more, so what the loop sends fails, but it may still send.
    peer_read_shut: bool,
    // The peer's handles: the peer's end shuts when the last goes.
    peers: usize,
}

// The loop's end of a pipe.
enum End {
    // Not yet accepted, on the listener at `listener`, with the events that the peer has made on
    // it, which count as the loop accepts it.
    Pending { listener: PathBuf, events: usize },
    Open,
    // Shut down or closed by the loop, or not yet accepted as its listener closed.
    Shut,
}

impl World {
    /// A world whose clock reads 0.
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                changed: Condvar::new(),
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        stdx::unpoisoned(self.shared.state.lock())
    }

    /// A backend for a loop in the world. As it drops, it closes what it holds, as the machine's
    /// closes its descriptors.
    ///
    /// # Panics
    ///
    /// Panics if another backend of the world lives.
    pub fn backend(&self) -> Box<dyn Backend> {
        let mut state = self.state();
        assert!(
            !state.looped,
            "a second backend of a world while its first lives: expected one loop in a world at a \
             time"
        );
        state.looped = true;
        state.woken = false;
        state.synced_events = state.events;
        drop(state);
        Box::new(Loop {
            world: self.clone(),
            settled: None,
        })
    }

    /// The time on the world's clock.
    pub fn now(&self) -> Duration {
        self.state().now
    }

    /// The wall-clock time that the world's clock reads now: the machine's, as it read when it was
    /// first asked, and the world's time since.
    pub fn realtime(&self) -> SystemTime {
        let mut state = self.state();
        let unix = *state.unix.get_or_insert_with(SystemTime::now);
        unix + state.now
    }

    /// Moves the clock to `at`, unless it is past it already.
    pub fn reach(&self, at: Duration) {
        let mut state = self.state();
        state.now = state.now.max(at);
        drop(state);
        self.shared.changed.notify_all();
    }

    /// Waits until the clock reaches `at`, which only another thread can move it to.
    pub fn wait_until(&self, at: Duration) {
        let mut state = self.state();
        while state.now < at {
            state = stdx::unpoisoned(self.shared.changed.wait(state));
        }
    }

    /// Tells `observe` of each sync, by its file's path and how much it makes last, as it begins.
    /// It runs under the world's lock, so it must not call the world.
    pub fn observe(&self, observe: Observer) {
        self.state().observers.push(observe);
    }

    /// Calls `call`, work that lasts a simulated time and sends the loop an event as it ends, once
    /// the clock reaches `at`, which the world moves it to as it does to complete a sync. Calls due
    /// at one time are called in the order that they were given. A call still due as the loop's
    /// backend drops is dropped with it.
    pub fn at(&self, at: Duration, call: impl FnOnce() + Send + 'static) {
        self.state().timed.push((at, Box::new(call)));
        self.shared.changed.notify_all();
    }

    /// Adds `script`, which the first sync that it matches meets, after the scripts before it.
    pub fn script(&self, script: Script) {
        self.state().scripts.push_back(script);
    }

    /// Connects to the listener at `path`, as a peer outside the loop does.
    pub fn connect(&self, path: &Path) -> io::Result<Peer> {
        let path = canonical(path)?;
        let mut state = self.state();
        if !state.listeners.contains_key(&path) {
            return Err(io::ErrorKind::ConnectionRefused.into());
        }
        let pipe = state.pipes.len();
        state.pipes.push(Pipe {
            to_loop: VecDeque::new(),
            to_peer: VecDeque::new(),
            end: End::Pending {
                listener: path,
                events: 0,
            },
            peer_shut: false,
            peer_read_shut: false,
            peers: 1,
        });
        drop(state);
        self.shared.changed.notify_all();
        Ok(Peer {
            world: self.clone(),
            pipe,
            moves: None,
        })
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

// A path by its directory's real path, as a socket's path may name its directory through a
// descriptor of it, and the socket itself is nowhere on disk.
fn canonical(path: &Path) -> io::Result<PathBuf> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::ErrorKind::InvalidInput.into());
    };
    Ok(fs::canonicalize(dir)?.join(name))
}

/// A peer's end of a stream in the world, which reads and writes as a blocking socket does.
///
/// A write sends all it is given, unless the stream ends first. If the loop's listener closes
/// before accepting the stream, the stream ends: a read reads the end of the stream, and a write
/// fails with `BrokenPipe`.
pub struct Peer {
    world: World,
    pipe: usize,
    // Where the clock moves to as this handle next writes.
    moves: Option<Duration>,
}

impl Peer {
    pub fn try_clone(&self) -> io::Result<Self> {
        self.world.state().pipes[self.pipe].peers += 1;
        Ok(Self {
            world: self.world.clone(),
            pipe: self.pipe,
            moves: None,
        })
    }

    /// Moves the clock to `at` as this handle next writes, under the same lock, so that the loop
    /// takes what it writes at `at`, as a peer that slept until then and sent at once.
    pub fn move_on_write(&mut self, at: Duration) {
        self.moves = Some(at);
    }

    /// Ends the peer's end both ways, for every handle of it.
    pub fn shutdown(&self) {
        self.world.state().peer_ends(self.pipe);
        self.world.shared.changed.notify_all();
    }

    /// Ends the peer's reading only: what the loop sends then fails.
    pub fn shutdown_read(&self) {
        let mut state = self.world.state();
        let pipe = &mut state.pipes[self.pipe];
        pipe.to_peer.clear();
        pipe.peer_read_shut = true;
        drop(state);
        self.world.shared.changed.notify_all();
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let mut state = self.world.state();
        let pipe = &mut state.pipes[self.pipe];
        pipe.peers -= 1;
        if pipe.peers == 0 {
            state.peer_ends(self.pipe);
        }
        drop(state);
        self.world.shared.changed.notify_all();
    }
}

impl Read for Peer {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut state = self.world.state();
        loop {
            let pipe = &mut state.pipes[self.pipe];
            if !pipe.to_peer.is_empty() {
                let read = buf.len().min(pipe.to_peer.len());
                for (byte, taken) in buf.iter_mut().zip(pipe.to_peer.drain(..read)) {
                    *byte = taken;
                }
                drop(state);
                self.world.shared.changed.notify_all();
                return Ok(read);
            }
            let ended = match pipe.end {
                End::Pending { .. } | End::Open => pipe.peer_shut || pipe.peer_read_shut,
                End::Shut => true,
            };
            if ended {
                return Ok(0);
            }
            state = stdx::unpoisoned(self.world.shared.changed.wait(state));
        }
    }
}

impl Write for Peer {
    // As a blocking socket on Linux sends, all of `buf`, waiting for room as it must, unless the
    // stream ends first, which leaves what went written:
    // https://github.com/torvalds/linux/blob/adc218676eef25575469234709c2d87185ca223a/net/unix/af_unix.c#L2282-L2371
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self.world.state();
        let mut written = 0;
        loop {
            let pipe = &mut state.pipes[self.pipe];
            let open = match pipe.end {
                End::Pending { .. } | End::Open => !pipe.peer_shut,
                End::Shut => false,
            };
            if !open && written == 0 {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            if !open || written == buf.len() {
                break;
            }
            let room = STREAM_BYTES.saturating_sub(pipe.to_loop.len());
            if room == 0 {
                state = stdx::unpoisoned(self.world.shared.changed.wait(state));
                continue;
            }
            let sent = room.min(buf.len() - written);
            pipe.to_loop.extend(&buf[written..written + sent]);
            written += sent;
            if let Some(at) = self.moves.take() {
                state.now = state.now.max(at);
            }
            self.world.shared.changed.notify_all();
        }
        if written > 0 {
            state.reached(self.pipe);
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A loop's backend in the world.
struct Loop {
    world: World,
    // The time at which the last wait settled what it handed over, for `now` to read once after
    // it: the clock may have moved since, but what the wait handed over completed then.
    settled: Option<Duration>,
}

impl State {
    fn hold(&mut self, object: Object) -> Handle {
        let index = self.free.pop().unwrap_or_else(|| {
            self.objects.push((0, None));
            u32::try_from(self.objects.len() - 1).unwrap()
        });
        let (generation, slot) = &mut self.objects[index as usize];
        *slot = Some(object);
        Handle {
            index,
            generation: *generation,
        }
    }

    fn object(&self, handle: Handle) -> &Object {
        match self.objects.get(handle.index as usize) {
            Some((generation, Some(object))) if *generation == handle.generation => object,
            _ => panic!("{handle:?} names nothing the world holds"),
        }
    }

    // Takes what `handle` names, which it names no longer.
    fn release(&mut self, handle: Handle) -> Object {
        let held = match self.objects.get_mut(handle.index as usize) {
            Some((generation, slot)) if *generation == handle.generation => slot.take(),
            _ => None,
        };
        let Some(object) = held else {
            panic!("{handle:?} names nothing the world holds")
        };
        let (generation, _) = &mut self.objects[handle.index as usize];
        *generation = generation.wrapping_add(1);
        self.free.push(handle.index);
        object
    }

    // Ends what the loop held: a listener ends each stream it hadn't accepted, and a stream ends
    // the loop's end.
    fn end(&mut self, object: Object) {
        match object {
            Object::Listener(path) => {
                self.listeners.remove(&path);
                for pipe in &mut self.pipes {
                    if matches!(&pipe.end, End::Pending { listener, .. } if *listener == path) {
                        pipe.end = End::Shut;
                    }
                }
            }
            Object::Stream(pipe) => self.pipes[pipe].end = End::Shut,
            Object::File(_) => {}
        }
    }

    // Counts what the peer did on `pipe` as an event that reached the loop: now if the loop holds
    // the stream, or as it accepts it.
    fn reached(&mut self, pipe: usize) {
        match &mut self.pipes[pipe].end {
            End::Open => self.events += 1,
            End::Pending { events, .. } => *events += 1,
            End::Shut => {}
        }
    }

    // Ends the peer's end of `pipe`, for every handle of it, which counts once.
    fn peer_ends(&mut self, pipe: usize) {
        if !std::mem::replace(&mut self.pipes[pipe].peer_shut, true) {
            self.reached(pipe);
        }
    }

    // Tries `op`: its outcome, or `op` back if it waits.
    fn attempt(&mut self, op: Op) -> Result<Outcome, Op> {
        match op {
            Op::Accept(handle) => {
                let Object::Listener(path) = self.object(handle) else {
                    panic!("an accept on {handle:?}, which is no listener")
                };
                let path = path.clone();
                let waiting = (self.pipes.iter().enumerate()).find_map(|(pipe, waiting)| {
                    match &waiting.end {
                        End::Pending { listener, events } if *listener == path => {
                            Some((pipe, *events))
                        }
                        End::Pending { .. } | End::Open | End::Shut => None,
                    }
                });
                let Some((pipe, events)) = waiting else {
                    return Err(Op::Accept(handle));
                };
                self.pipes[pipe].end = End::Open;
                self.events += events;
                Ok(Outcome::Accepted(Ok(self.hold(Object::Stream(pipe)))))
            }
            Op::Recv(handle, mut buffer) => {
                let pipe = self.pipe(handle);
                let pipe = &mut self.pipes[pipe];
                if pipe.to_loop.is_empty() {
                    if pipe.peer_shut || !matches!(pipe.end, End::Open) {
                        return Ok(Outcome::Received(buffer, Ok(0)));
                    }
                    return Err(Op::Recv(handle, buffer));
                }
                let read = buffer.len().min(pipe.to_loop.len());
                for (byte, taken) in buffer.iter_mut().zip(pipe.to_loop.drain(..read)) {
                    *byte = taken;
                }
                Ok(Outcome::Received(buffer, Ok(read)))
            }
            Op::Send {
                handle,
                buffer,
                from,
            } => {
                let pipe = self.pipe(handle);
                let pipe = &mut self.pipes[pipe];
                if pipe.peer_shut || pipe.peer_read_shut || !matches!(pipe.end, End::Open) {
                    let broken = io::ErrorKind::BrokenPipe.into();
                    return Ok(Outcome::Sent(buffer, Err(broken)));
                }
                let room = STREAM_BYTES.saturating_sub(pipe.to_peer.len());
                if room == 0 {
                    return Err(Op::Send {
                        handle,
                        buffer,
                        from,
                    });
                }
                let sent = room.min(buffer.len() - from);
                pipe.to_peer.extend(&buffer[from..from + sent]);
                Ok(Outcome::Sent(buffer, Ok(sent)))
            }
            Op::Timeout(at) if self.now >= at => Ok(Outcome::TimedOut),
            op @ (Op::Fsync(..) | Op::Timeout(_)) => Err(op),
            Op::TryLock(handle) => {
                let Object::File(path) = self.object(handle) else {
                    panic!("a lock of {handle:?}, which is no file")
                };
                let held = *self.locks.entry(path.clone()).or_insert(handle);
                Ok(Outcome::Locked(Ok(held == handle)))
            }
        }
    }

    fn pipe(&self, handle: Handle) -> usize {
        let Object::Stream(pipe) = self.object(handle) else {
            panic!("a stream's operation on {handle:?}, which is no stream")
        };
        *pipe
    }

    // Moves what may now complete of what waits to the outcomes ready: whether it moved any, which
    // may have changed a stream that a peer waits on.
    fn settle(&mut self) -> bool {
        let ready = self.ready.len();
        for (slot, op) in std::mem::take(&mut self.parked) {
            match self.attempt(op) {
                Ok(outcome) => self.ready.push((slot, outcome)),
                Err(op) => self.parked.push((slot, op)),
            }
        }
        let now = self.now;
        let events = self.events;
        for (slot, synced, began, path) in std::mem::take(&mut self.held) {
            let done = match &synced {
                Synced::Fails => true,
                Synced::Lasts { at, events: more } => events >= began + more && now >= *at,
                Synced::Held(until) => until(&path),
            };
            if done {
                let outcome = match synced {
                    Synced::Fails => Err(io::Error::other("the simulated host's sync failed")),
                    Synced::Lasts { .. } | Synced::Held(_) => Ok(()),
                };
                self.ready.push((slot, Outcome::Synced(outcome)));
            } else {
                self.held.push((slot, synced, began, path));
            }
        }
        self.ready.len() > ready
    }

    // The earliest time that a sync whose events have been sent, a timeout or a timed call waits
    // to complete at.
    fn next_due(&self) -> Option<Duration> {
        let events = self.events;
        let syncs = (self.held.iter()).filter_map(|(_, synced, began, _)| match synced {
            Synced::Lasts { at, events: more } if events >= began + more => Some(*at),
            Synced::Lasts { .. } | Synced::Fails | Synced::Held(_) => None,
        });
        let timeouts = (self.parked.iter()).filter_map(|(_, op)| match op {
            Op::Timeout(at) => Some(*at),
            Op::Accept(_) | Op::Recv(..) | Op::Send { .. } | Op::Fsync(..) | Op::TryLock(_) => None,
        });
        let timed = self.timed.iter().map(|(at, _)| *at);
        syncs.chain(timeouts).chain(timed).min()
    }
}

impl Backend for Loop {
    fn now(&mut self) -> Duration {
        self.settled.take().unwrap_or_else(|| self.world.now())
    }

    fn listen(&mut self, path: &Path, _: u32) -> io::Result<Handle> {
        let path = canonical(path)?;
        let mut state = self.world.state();
        if state.listeners.contains_key(&path) {
            return Err(io::ErrorKind::AddrInUse.into());
        }
        let handle = state.hold(Object::Listener(path.clone()));
        state.listeners.insert(path, handle.index);
        Ok(handle)
    }

    fn adopt(&mut self, _: OwnedFd) -> io::Result<Handle> {
        Err(io::Error::other(
            "the simulated world meets sandboxes only on its sockets in memory, never over the \
             machine's network",
        ))
    }

    fn file(&mut self, file: fs::File) -> Handle {
        // A file knows no path of its own, but the kernel's table of the process's files does.
        let path = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).unwrap();
        self.world.state().hold(Object::File(path))
    }

    fn shutdown(&mut self, handle: Handle) {
        let mut state = self.world.state();
        let pipe = state.pipe(handle);
        state.pipes[pipe].end = End::Shut;
        drop(state);
        self.world.shared.changed.notify_all();
    }

    fn close(&mut self, handle: Handle) {
        let mut state = self.world.state();
        let object = state.release(handle);
        state.end(object);
        state.locks.retain(|_, holder| *holder != handle);
        let parked = std::mem::take(&mut state.parked);
        for (slot, op) in parked {
            // A lock completes at once, and a timeout waits on the clock, not on a handle.
            let on = match &op {
                Op::Accept(on) | Op::Recv(on, _) | Op::Fsync(on, _) => Some(*on),
                Op::Send { handle, .. } => Some(*handle),
                Op::TryLock(_) | Op::Timeout(_) => None,
            };
            if on == Some(handle) {
                let aborted = || io::Error::from(io::ErrorKind::ConnectionAborted);
                let outcome = match op {
                    Op::Accept(_) => Outcome::Accepted(Err(aborted())),
                    Op::Recv(_, buffer) => Outcome::Received(buffer, Err(aborted())),
                    Op::Send { buffer, .. } => Outcome::Sent(buffer, Err(aborted())),
                    Op::Fsync(..) => Outcome::Synced(Err(aborted())),
                    Op::TryLock(_) | Op::Timeout(_) => {
                        unreachable!("{op:?} was aborted, though it waits on no handle")
                    }
                };
                state.ready.push((slot, outcome));
            } else {
                state.parked.push((slot, op));
            }
        }
        drop(state);
        self.world.shared.changed.notify_all();
    }

    fn submit(&mut self, slot: Slot, op: Op) {
        let mut state = self.world.state();
        if let Op::Fsync(handle, durable) = op {
            let Object::File(path) = state.object(handle) else {
                panic!("a sync of {handle:?}, which is no file")
            };
            let path = path.clone();
            for observe in &state.observers {
                observe(&path, durable);
            }
            let script = (state.scripts.iter()).position(|script| (script.matches)(&path));
            let Some(script) = script.and_then(|at| state.scripts.remove(at)) else {
                state.synced_events = state.events;
                state.ready.push((slot, Outcome::Synced(Ok(()))));
                return;
            };
            let events = state.events;
            let began = std::mem::replace(&mut state.synced_events, events);
            state.held.push((slot, script.synced, began, path));
            return;
        }
        match state.attempt(op) {
            Ok(outcome) => state.ready.push((slot, outcome)),
            Err(op) => state.parked.push((slot, op)),
        }
        drop(state);
        self.world.shared.changed.notify_all();
    }

    fn wait(&mut self, until: Duration, owed: bool, done: &mut Vec<(Slot, Outcome)>) {
        let mut state = self.world.state();
        // Whether the wait changed what another thread may wait on: a stream or the clock.
        let mut changed = false;
        let mut idled = false;
        loop {
            changed |= state.settle();
            let now = state.now;
            let due: Vec<Timed> = (state.timed.extract_if(.., |(at, _)| *at <= now))
                .map(|(_, call)| call)
                .collect();
            if !due.is_empty() {
                // Each sends the loop an event, whose wake takes the world's lock.
                drop(state);
                for call in due {
                    call();
                }
                state = self.world.state();
                changed = true;
                continue;
            }
            let woken = std::mem::take(&mut state.woken);
            if idled || woken || !state.ready.is_empty() || state.now >= until {
                break;
            }
            // An edge worker that owes the loop its event runs at no simulated time.
            if !owed && let Some(due) = state.next_due() {
                // On its way to what completes next, the clock stops at what the loop waits until.
                // What completes then completes under the same lock, before another thread can
                // act at the new time.
                state.now = state.now.max(due.min(until));
                changed = true;
                continue;
            }
            let waited = self.world.shared.changed.wait_timeout(state, IDLE);
            state = stdx::unpoisoned(waited).0;
            idled = true;
        }
        done.append(&mut state.ready);
        self.settled = Some(state.now);
        drop(state);
        if changed {
            self.world.shared.changed.notify_all();
        }
    }

    fn waker(&self) -> Arc<dyn Fn() + Send + Sync> {
        let world = self.world.clone();
        Arc::new(move || {
            let mut state = world.state();
            state.woken = true;
            state.events += 1;
            drop(state);
            world.shared.changed.notify_all();
        })
    }
}

impl Drop for Loop {
    fn drop(&mut self) {
        // A backend dropped as its thread panics has failed already, and a second panic would
        // abort a test's process before the first is reported.
        if std::thread::panicking() {
            return;
        }
        let mut state = self.world.state();
        let objects = std::mem::take(&mut state.objects);
        for object in objects.into_iter().filter_map(|(_, object)| object) {
            state.end(object);
        }
        state.free.clear();
        state.parked.clear();
        state.ready.clear();
        state.held.clear();
        state.locks.clear();
        // Dropped once the lock is let go, as what a call holds may wake the loop as it drops.
        let timed = std::mem::take(&mut state.timed);
        state.looped = false;
        drop(state);
        drop(timed);
        self.world.shared.changed.notify_all();
    }
}
