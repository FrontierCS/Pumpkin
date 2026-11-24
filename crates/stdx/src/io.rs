//! One door to the operating system for a process's loop: operations that complete later, each
//! with a callback that runs only in the loop's batch, never inside the call that submitted it.
//!
//! An [`Io`] is owned by one thread, whose loop alternates its owner's tick with
//! [`Io::run_until`]. Each batch reads the time once ([`Io::now`]), then runs the callback of each
//! operation that completed, then hands the owner each [`Loop::Event`] that other threads had sent
//! by then ([`Io::events`]), then runs the callbacks that the batch before asked for
//! ([`Io::next_tick`]). A callback gets the owner by `&mut`, with the `Io` beside it, and the token
//! given at submission, by which the owner finds the part that waits on it.
//!
//! A [`Backend`] performs the operations: the machine's ([`machine`], epoll) or a simulated one,
//! which yields only outcomes the machine's can. The operations in flight are bounded by the
//! capacity the owner gives, and each owns its buffer until its callback gets it back.
// The model is TigerBeetle's IO: completions whose callbacks run in `run_for_ns`, a fixed set of
// them, and one interface that a simulator can stand behind:
// https://github.com/tigerbeetle/tigerbeetle/blob/68958eb757747d0fc96a79d1f9c2fa6766b67b48/src/stdx/io/linux.zig#L100-L117
// https://github.com/tigerbeetle/tigerbeetle/blob/68958eb757747d0fc96a79d1f9c2fa6766b67b48/src/stdx/io/linux.zig#L377-L388

use std::collections::BTreeSet;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;
use std::{fs, io, mem};

mod epoll;

/// The owner of an [`Io`], whose callbacks it runs.
pub trait Loop: Sized {
    /// What other threads send the loop.
    type Event: Send + 'static;

    /// Takes an event that another thread sent, in the batch after it was sent.
    fn event(&mut self, io: &mut Io<Self>, event: Self::Event);
}

/// A backend's name for a listener, stream or file that it holds, until it is closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Handle {
    pub index: u32,
    pub generation: u32,
}

/// A slot of the operations in flight, which names an operation to its backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot(pub u32);

/// An operation, as a backend performs it. A buffer goes back with its outcome.
#[derive(Debug)]
pub enum Op {
    /// Accepts the next connection to a listener.
    Accept(Handle),
    /// Receives into the whole buffer, at most its length.
    Recv(Handle, Vec<u8>),
    /// Sends the buffer from `from`, all of it or a part.
    Send {
        handle: Handle,
        buffer: Vec<u8>,
        from: usize,
    },
    /// Makes what was written to a file last through a crash of the machine.
    Fsync(Handle, Durable),
    /// Takes a file's lock, as `flock`'s exclusive lock, if no other open file of it holds one.
    TryLock(Handle),
    /// Waits until the backend's clock reads this time.
    Timeout(Duration),
}

/// What an [`Op`] came to.
#[derive(Debug)]
pub enum Outcome {
    Accepted(io::Result<Handle>),
    /// The buffer, and how many bytes it holds from its start; 0 at the end of the stream.
    Received(Vec<u8>, io::Result<usize>),
    /// The buffer, and how many bytes from `from` went.
    Sent(Vec<u8>, io::Result<usize>),
    Synced(io::Result<()>),
    /// Whether the lock was taken: `false` while another open file of it holds one.
    Locked(io::Result<bool>),
    TimedOut,
}

/// How much of a file an [`Op::Fsync`] makes last.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durable {
    /// Its data, and what of its metadata reading the data back needs, such as its size, as
    /// `fdatasync` makes last.
    Data,
    /// Its data and all its metadata, and a directory's entries, as `fsync` makes last.
    All,
}

/// What performs an [`Io`]'s operations.
pub trait Backend: Send {
    /// The monotonic time since the backend's origin, which never goes back.
    fn now(&mut self) -> Duration;

    /// Listens on a Unix socket at `path`, which must not exist, with `mode` as its permission
    /// bits.
    fn listen(&mut self, path: &Path, mode: u32) -> io::Result<Handle>;

    /// Holds a connected stream socket that the process made another way.
    fn adopt(&mut self, stream: OwnedFd) -> io::Result<Handle>;

    /// Holds an open file of the process's.
    fn file(&mut self, file: fs::File) -> Handle;

    /// Shuts a stream down both ways: what waits on it completes, and receives return what had
    /// arrived, then the end of the stream.
    fn shutdown(&mut self, handle: Handle);

    /// Closes what `handle` names: an operation still waiting on it completes as aborted.
    fn close(&mut self, handle: Handle);

    /// Performs `op`, whose outcome a later [`Backend::wait`] hands over with `slot`, once: an
    /// outcome of `op`'s own kind, with its buffer. Operations complete in any order, and a sync
    /// may last through any number of waits, as a disk may. A socket has one read, an accept or a
    /// receive, and one send in flight at most, until its outcome is handed over.
    fn submit(&mut self, slot: Slot, op: Op);

    /// Adds to `done` what has completed, waiting for at least one completion until `until`, a time
    /// on [`Backend::now`], or until [`Backend::waker`]'s wake. It may return early with nothing.
    /// While `owed`, another thread owes the loop an event ([`Io::edge`]), so a clock that the
    /// backend moves itself stays where it is.
    fn wait(&mut self, until: Duration, owed: bool, done: &mut Vec<(Slot, Outcome)>);

    /// What ends a [`Backend::wait`] from another thread.
    fn waker(&self) -> Arc<dyn Fn() + Send + Sync>;
}

/// A listener on a Unix socket, which the loop holds until [`Io::close_listener`].
#[derive(Debug, PartialEq, Eq)]
#[must_use = "a listener is held until it is closed"]
pub struct Listener(Handle);

/// A connected stream socket, which the loop holds until [`Io::close`].
#[derive(Debug, PartialEq, Eq)]
#[must_use = "a stream is held until it is closed"]
pub struct Stream(Handle);

/// An open file, which the loop holds until [`Io::close_file`].
#[derive(Debug, PartialEq, Eq)]
#[must_use = "a file is held until it is closed"]
pub struct File(Handle);

/// The callback that an operation completes with: the owner, the loop, the token given with the
/// operation, and the outcome. A buffer comes back with how many of its bytes the operation took.
pub type OnAccept<C> = fn(&mut C, &mut Io<C>, u64, io::Result<Stream>);
pub type OnBytes<C> = fn(&mut C, &mut Io<C>, u64, Vec<u8>, io::Result<usize>);
pub type OnSync<C> = fn(&mut C, &mut Io<C>, u64, io::Result<()>);
pub type OnLock<C> = fn(&mut C, &mut Io<C>, u64, io::Result<bool>);
pub type OnTick<C> = fn(&mut C, &mut Io<C>, u64);

enum Callback<C: Loop> {
    Accept(OnAccept<C>),
    Bytes(OnBytes<C>),
    Sync(OnSync<C>),
    Lock(OnLock<C>),
    Tick(OnTick<C>),
}

struct Pending<C: Loop> {
    token: u64,
    callback: Callback<C>,
    way: Option<Way>,
}

// What a socket's operation waits on: the socket to read from, by an accept or a receive, or to
// write to, by a send. One of each at most is in flight on a socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Way {
    Read(Handle),
    Write(Handle),
}

/// A loop's operations and the events sent to it, owned by the loop's thread.
pub struct Io<C: Loop> {
    backend: Box<dyn Backend>,
    // The operations in flight, by slot, bounded by the capacity given.
    pending: Vec<Option<Pending<C>>>,
    free: Vec<u32>,
    done: Vec<(Slot, Outcome)>,
    // What the operations in flight on sockets wait on.
    ways: BTreeSet<Way>,
    // The callbacks for the next batch, which runs without waiting.
    ticks: Vec<(u64, OnTick<C>)>,
    now: Duration,
    // What other threads send the loop, and its own sender, kept so that the loop can hand out
    // senders, and its receiver never disconnects.
    received: Receiver<Envelope<C::Event>>,
    events: Events<C::Event>,
    // How many events the loop's edge workers owe it ([`Io::edge`]).
    owed: usize,
}

// An event, or the end of an edge worker that sent none, and whether an edge worker sent it.
struct Envelope<E> {
    event: Option<E>,
    edge: bool,
}

/// The machine's backend, on epoll.
pub fn machine() -> io::Result<Box<dyn Backend>> {
    Ok(Box::new(epoll::Epoll::new()?))
}

/// Sends a loop events from other threads ([`Loop::event`]).
pub struct Events<E> {
    sender: Sender<Envelope<E>>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl<E> Clone for Events<E> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            wake: Arc::clone(&self.wake),
        }
    }
}

impl<E> Events<E> {
    /// Sends `event`, which the loop takes in its next batch. An event sent to a loop that has
    /// ended is dropped.
    pub fn send(&self, event: E) {
        let envelope = Envelope {
            event: Some(event),
            edge: false,
        };
        if self.sender.send(envelope).is_ok() {
            (self.wake)();
        }
    }
}

/// The one event that an edge worker owes its loop, which the loop counts as in flight until it
/// takes it: [`Edge::send`] sends it, and a drop unsent ends the debt with no event.
pub struct Edge<E> {
    sender: Sender<Envelope<E>>,
    wake: Arc<dyn Fn() + Send + Sync>,
    sent: bool,
}

impl<E> Edge<E> {
    pub fn send(mut self, event: E) {
        self.sent = true;
        self.post(Some(event));
    }

    fn post(&self, event: Option<E>) {
        let envelope = Envelope { event, edge: true };
        if self.sender.send(envelope).is_ok() {
            (self.wake)();
        }
    }
}

impl<E> Drop for Edge<E> {
    fn drop(&mut self) {
        if !self.sent {
            self.post(None);
        }
    }
}

impl<C: Loop> Io<C> {
    /// A loop on `backend`, with at most `capacity` operations in flight.
    pub fn new(mut backend: Box<dyn Backend>, capacity: usize) -> Self {
        let Ok(top) = u32::try_from(capacity) else {
            unreachable!("a loop's capacity of {capacity} operations passes a u32's")
        };
        let now = backend.now();
        let (sender, received) = mpsc::channel();
        let wake = backend.waker();
        let events = Events { sender, wake };
        Self {
            backend,
            pending: (0..capacity).map(|_| None).collect(),
            free: (0..top).rev().collect(),
            done: Vec::new(),
            ways: BTreeSet::new(),
            ticks: Vec::new(),
            now,
            received,
            events,
            owed: 0,
        }
    }

    /// The time of this batch, on the backend's monotonic clock: every callback of a batch reads
    /// the same.
    pub fn now(&self) -> Duration {
        self.now
    }

    /// What an edge worker, a thread that waits on what the loop doesn't, such as a blocking call
    /// of another library, completes into the loop with: the loop counts its event as in flight
    /// until it takes it, which a simulated clock waits for.
    pub fn edge(&mut self) -> Edge<C::Event> {
        self.owed += 1;
        Edge {
            sender: self.events.sender.clone(),
            wake: Arc::clone(&self.events.wake),
            sent: false,
        }
    }

    pub fn events(&self) -> Events<C::Event> {
        self.events.clone()
    }

    /// Waits for the next batch, until `until`, a time on [`Io::now`], at most, and runs it.
    pub fn run_until(&mut self, owner: &mut C, until: Duration) {
        let mut done = mem::take(&mut self.done);
        let ticks = mem::take(&mut self.ticks);
        let until = if ticks.is_empty() { until } else { self.now };
        self.backend.wait(until, self.owed > 0, &mut done);
        self.now = self.backend.now();
        for (Slot(slot), outcome) in done.drain(..) {
            let index = slot as usize;
            let Some(Pending {
                token,
                callback,
                way,
            }) = self.pending[index].take()
            else {
                unreachable!(
                    "slot {slot} completed with nothing in flight, though a backend completes \
                     each operation it was given once"
                )
            };
            self.free.push(slot);
            if let Some(way) = way {
                self.ways.remove(&way);
            }
            match (callback, outcome) {
                (Callback::Accept(callback), Outcome::Accepted(accepted)) => {
                    callback(owner, self, token, accepted.map(Stream));
                }
                (
                    Callback::Bytes(callback),
                    Outcome::Received(buffer, result) | Outcome::Sent(buffer, result),
                ) => callback(owner, self, token, buffer, result),
                (Callback::Sync(callback), Outcome::Synced(result)) => {
                    callback(owner, self, token, result);
                }
                (Callback::Lock(callback), Outcome::Locked(locked)) => {
                    callback(owner, self, token, locked);
                }
                (Callback::Tick(callback), Outcome::TimedOut) => callback(owner, self, token),
                (
                    Callback::Accept(_)
                    | Callback::Bytes(_)
                    | Callback::Sync(_)
                    | Callback::Lock(_)
                    | Callback::Tick(_),
                    outcome,
                ) => {
                    unreachable!(
                        "slot {slot} completed with {outcome:?}, which is no outcome of its \
                         operation, though a backend answers each operation with its own kind"
                    )
                }
            }
        }
        self.done = done;
        // Only what was sent by now, so that an owner that sends to its own loop as it takes an
        // event can't hold the batch.
        let envelopes: Vec<Envelope<C::Event>> = self.received.try_iter().collect();
        for Envelope { event, edge } in envelopes {
            if edge {
                self.owed -= 1;
            }
            if let Some(event) = event {
                owner.event(self, event);
            }
        }
        for (token, callback) in ticks {
            callback(owner, self, token);
        }
    }

    /// Runs `callback` with `token` in the next batch, which then waits for nothing.
    pub fn next_tick(&mut self, token: u64, callback: OnTick<C>) {
        self.ticks.push((token, callback));
    }

    /// Listens on a Unix socket at `path`, which must not exist, with `mode` as its permission
    /// bits.
    pub fn listen(&mut self, path: &Path, mode: u32) -> io::Result<Listener> {
        self.backend.listen(path, mode).map(Listener)
    }

    pub fn adopt(&mut self, stream: OwnedFd) -> io::Result<Stream> {
        self.backend.adopt(stream).map(Stream)
    }

    pub fn file(&mut self, file: fs::File) -> File {
        File(self.backend.file(file))
    }

    /// Shuts `stream` down both ways ([`Backend::shutdown`]).
    pub fn shutdown(&mut self, stream: &Stream) {
        self.backend.shutdown(stream.0);
    }

    /// Closes `stream`: an operation still waiting on it completes as aborted.
    pub fn close(&mut self, Stream(handle): Stream) {
        self.backend.close(handle);
    }

    pub fn close_listener(&mut self, Listener(handle): Listener) {
        self.backend.close(handle);
    }

    pub fn close_file(&mut self, File(handle): File) {
        self.backend.close(handle);
    }

    /// Accepts the next connection to `listener`, on which no other accept is in flight.
    pub fn accept(&mut self, listener: &Listener, token: u64, callback: OnAccept<C>) {
        let way = Some(Way::Read(listener.0));
        self.submit(
            Op::Accept(listener.0),
            way,
            token,
            Callback::Accept(callback),
        );
    }

    /// Receives into `buffer` on `stream`.
    ///
    /// # Panics
    ///
    /// Panics if `buffer` is empty, or another receive is in flight on `stream`.
    pub fn recv(&mut self, stream: &Stream, buffer: Vec<u8>, token: u64, callback: OnBytes<C>) {
        assert!(
            !buffer.is_empty(),
            "a receive into an empty buffer: expected room for a byte, as 0 bytes is the end of \
             the stream"
        );
        let way = Some(Way::Read(stream.0));
        self.submit(
            Op::Recv(stream.0, buffer),
            way,
            token,
            Callback::Bytes(callback),
        );
    }

    /// Sends `buffer` from `from` on `stream`.
    ///
    /// # Panics
    ///
    /// Panics if `from` leaves no byte to send, or another send is in flight on `stream`.
    pub fn send(
        &mut self,
        stream: &Stream,
        (buffer, from): (Vec<u8>, usize),
        token: u64,
        callback: OnBytes<C>,
    ) {
        assert!(
            from < buffer.len(),
            "a send from byte {from} of {}: expected a byte to send at least",
            buffer.len()
        );
        let handle = stream.0;
        let op = Op::Send {
            handle,
            buffer,
            from,
        };
        self.submit(
            op,
            Some(Way::Write(handle)),
            token,
            Callback::Bytes(callback),
        );
    }

    pub fn fsync(&mut self, file: &File, durable: Durable, token: u64, callback: OnSync<C>) {
        self.submit(
            Op::Fsync(file.0, durable),
            None,
            token,
            Callback::Sync(callback),
        );
    }

    /// Takes `file`'s lock, as `flock`'s exclusive lock, if no other open file of its path holds
    /// one: `true` once taken, which `file` holds until it is closed, as it does if it held it
    /// already. It never waits for the lock to be let go, but completes in the next batch.
    // A `flock` with `LOCK_NB`, as TigerBeetle's IO locks its data file, which it tries again after
    // a sleep where a caller here submits a timeout:
    // https://github.com/tigerbeetle/tigerbeetle/blob/68958eb757747d0fc96a79d1f9c2fa6766b67b48/src/stdx/io/linux.zig#L1579-L1605
    pub fn try_lock(&mut self, file: &File, token: u64, callback: OnLock<C>) {
        self.submit(Op::TryLock(file.0), None, token, Callback::Lock(callback));
    }

    /// Runs `callback` with `token` in a batch whose time, [`Io::now`], is `at` or later, once the
    /// backend's clock has reached `at`.
    // As TigerBeetle's `timeout`, but at a time on the loop's clock rather than after a span:
    // https://github.com/tigerbeetle/tigerbeetle/blob/68958eb757747d0fc96a79d1f9c2fa6766b67b48/src/stdx/io/linux.zig#L1226-L1254
    pub fn timeout(&mut self, at: Duration, token: u64, callback: OnTick<C>) {
        self.submit(Op::Timeout(at), None, token, Callback::Tick(callback));
    }

    fn submit(&mut self, op: Op, way: Option<Way>, token: u64, callback: Callback<C>) {
        let Some(slot) = self.free.pop() else {
            unreachable!(
                "more than {} operations were put in flight, the loop's capacity, though its \
                 owner bounds them by it",
                self.pending.len()
            )
        };
        if let Some(way) = way {
            assert!(
                self.ways.insert(way),
                "{way:?} was submitted twice: expected one read and one write in flight on a \
                 socket at most"
            );
        }
        self.pending[slot as usize] = Some(Pending {
            token,
            callback,
            way,
        });
        self.backend.submit(Slot(slot), op);
    }
}

impl<C: Loop> Drop for Io<C> {
    fn drop(&mut self) {
        // A loop dropped as its thread panics has failed already, and a second panic would abort
        // a test's process before the first is reported.
        if std::thread::panicking() {
            return;
        }
        let in_flight = self
            .pending
            .iter()
            .filter(|pending| pending.is_some())
            .count();
        assert!(
            in_flight == 0,
            "a loop ended with {in_flight} operations in flight: expected its owner to wait for \
             each to complete"
        );
    }
}
