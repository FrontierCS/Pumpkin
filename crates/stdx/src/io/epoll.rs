//! The machine's backend: sockets on edge-triggered epoll, and files, which epoll can't wait on,
//! synced on a thread of the backend's, which completes each sync into the loop. A file's lock is
//! tried as it is submitted, and a timeout completes at the first wait that ends past its time.
// A socket is registered once, for both directions, and an operation is tried as it is submitted
// and again at each edge until it would no longer block, as TigerBeetle's epoll backend retries at
// each wakeup:
// https://github.com/tigerbeetle/tigerbeetle/blob/68958eb757747d0fc96a79d1f9c2fa6766b67b48/src/stdx/io/linux_epoll.zig#L924-L955
// Files are synced on a thread, so that a slow disk holds no socket's operations, as libuv and
// libxev's epoll backend run what epoll can't wait on in a pool of threads:
// https://github.com/libuv/libuv/blob/0c1fa696aa502eb749c2c4735005f41ba00a27b8/src/unix/fs.c#L150-L166
// https://github.com/mitchellh/libxev/blob/59ca768ca869e51c277ede3dc19ff73a89875f81/src/backend/epoll.zig#L36-L37

use std::fs::{self, TryLockError};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rustix::event::{EventfdFlags, epoll};
use rustix::net::{RecvFlags, SendFlags, Shutdown, SocketFlags};

use super::{Backend, Durable, Handle, Op, Outcome, Slot};
use crate::time::{Clock as _, Machine};

// The most readiness events that one wait takes; the rest wait for the next.
const EVENTS: usize = 256;

// The longest one wait blocks, so that its loop looks for a signal that another thread took,
// which `stdx::signal` notes and no descriptor tells of. A stop that comes this late goes
// unnoticed by the person who asked for it, and the loop wakes for it at most 20 times a second.
const LOOK: Duration = Duration::from_millis(50);

// A wait's timeout is its nanoseconds alone.
const _: () = assert!(LOOK.as_secs() == 0);

// The epoll data of the eventfd that wakes a wait, which no handle's index reaches.
const WAKE: u64 = u64::MAX;

pub(super) struct Epoll {
    fd: OwnedFd,
    wake: Arc<OwnedFd>,
    entries: Vec<Entry>,
    free: Vec<u32>,
    // What completed without waiting on epoll, for the next wait to hand over.
    ready: Vec<(Slot, Outcome)>,
    // Each timeout in flight, by the time it waits until.
    timeouts: Vec<(Duration, Slot)>,
    // The thread that syncs files, from the first sync on.
    syncer: Option<Syncer>,
}

struct Entry {
    generation: u32,
    held: Option<Held>,
}

struct Held {
    kind: Kind,
    // The operation that waits for the socket to be readable, and the one that waits for it to
    // be writable: one of each at most.
    reading: Option<(Slot, Op)>,
    writing: Option<(Slot, Op)>,
}

enum Kind {
    Listener(OwnedFd),
    Stream(OwnedFd),
    // Shared with the thread that syncs it.
    File(Arc<fs::File>),
}

// A sync for the syncer's thread to make.
type Job = (Slot, Arc<fs::File>, Durable);

// A thread that syncs files one after another, and wakes its loop's wait as each completes.
struct Syncer {
    jobs: Option<Sender<Job>>,
    synced: Receiver<(Slot, io::Result<()>)>,
    thread: Option<JoinHandle<()>>,
}

impl Syncer {
    fn start(wake: Arc<OwnedFd>) -> io::Result<Self> {
        let (jobs, taken) = mpsc::channel::<Job>();
        let (done, synced) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("io-sync".to_owned())
            .spawn(move || {
                for (slot, file, durable) in taken {
                    let result = match durable {
                        Durable::Data => file.sync_data(),
                        Durable::All => file.sync_all(),
                    };
                    if done.send((slot, result)).is_err() {
                        return;
                    }
                    wake_up(&wake);
                }
            })?;
        Ok(Self {
            jobs: Some(jobs),
            synced,
            thread: Some(thread),
        })
    }
}

impl Drop for Syncer {
    fn drop(&mut self) {
        // Its thread ends once it has made the syncs it took.
        drop(self.jobs.take());
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            unreachable!("the thread that syncs files panicked, though it only syncs them")
        }
    }
}

// Wakes a wait on epoll; a counter already at its most wakes it as well.
fn wake_up(wake: &OwnedFd) {
    let _ = rustix::io::write(wake, &1_u64.to_ne_bytes());
}

impl Epoll {
    pub(super) fn new() -> io::Result<Self> {
        let epoll = epoll::create(epoll::CreateFlags::CLOEXEC)?;
        let wake = rustix::event::eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?;
        let flags = epoll::EventFlags::IN | epoll::EventFlags::ET;
        epoll::add(&epoll, &wake, epoll::EventData::new_u64(WAKE), flags)?;
        Ok(Self {
            fd: epoll,
            wake: Arc::new(wake),
            entries: Vec::new(),
            free: Vec::new(),
            ready: Vec::new(),
            timeouts: Vec::new(),
            syncer: None,
        })
    }

    // Hands `file`'s sync to the syncer's thread, which it starts at the first sync.
    fn sync(&mut self, slot: Slot, file: Arc<fs::File>, durable: Durable) {
        let syncer = match &mut self.syncer {
            Some(syncer) => syncer,
            None => match Syncer::start(Arc::clone(&self.wake)) {
                Ok(syncer) => self.syncer.insert(syncer),
                Err(error) => {
                    let error = io::Error::new(
                        error.kind(),
                        format!("starting the thread that syncs files failed: {error}"),
                    );
                    return self.ready.push((slot, Outcome::Synced(Err(error))));
                }
            },
        };
        let sent =
            (syncer.jobs.as_ref()).is_some_and(|jobs| jobs.send((slot, file, durable)).is_ok());
        assert!(
            sent,
            "the thread that syncs files ended, though it ends only as the backend drops"
        );
    }

    // Moves the syncs that the syncer's thread has made to what is ready.
    fn take_synced(&mut self) {
        if let Some(syncer) = &self.syncer {
            let synced = syncer.synced.try_iter();
            self.ready
                .extend(synced.map(|(slot, result)| (slot, Outcome::Synced(result))));
        }
    }

    // Holds a listener's or a stream's socket, `kind` of `fd`, registered with epoll for both
    // directions.
    fn hold_socket(&mut self, fd: OwnedFd, kind: fn(OwnedFd) -> Kind) -> io::Result<Handle> {
        let index = self.vacant();
        let flags = epoll::EventFlags::IN
            | epoll::EventFlags::OUT
            | epoll::EventFlags::RDHUP
            | epoll::EventFlags::ET;
        let data = epoll::EventData::new_u64(u64::from(index));
        if let Err(error) = epoll::add(&self.fd, &fd, data, flags) {
            self.free.push(index);
            return Err(error.into());
        }
        Ok(self.hold_at(index, kind(fd)))
    }

    // An index of `entries` that holds nothing.
    fn vacant(&mut self) -> u32 {
        if let Some(index) = self.free.pop() {
            return index;
        }
        let Ok(index) = u32::try_from(self.entries.len()) else {
            unreachable!("a loop holds more handles than a u32 counts")
        };
        self.entries.push(Entry {
            generation: 0,
            held: None,
        });
        index
    }

    fn hold_at(&mut self, index: u32, kind: Kind) -> Handle {
        let entry = &mut self.entries[index as usize];
        entry.held = Some(Held {
            kind,
            reading: None,
            writing: None,
        });
        Handle {
            index,
            generation: entry.generation,
        }
    }

    fn held(&mut self, handle: Handle) -> &mut Held {
        let entry = self.entries.get_mut(handle.index as usize);
        match entry {
            Some(Entry {
                generation,
                held: Some(held),
            }) if *generation == handle.generation => held,
            _ => unreachable!(
                "{handle:?} names nothing the loop holds, though a handle is used only until it \
                 is closed"
            ),
        }
    }

    // Takes what `handle` names, which it no longer names.
    fn release(&mut self, handle: Handle) -> Held {
        let entry = self.entries.get_mut(handle.index as usize);
        let held = match entry {
            Some(Entry { generation, held }) if *generation == handle.generation => held.take(),
            _ => None,
        };
        let Some(held) = held else {
            unreachable!(
                "{handle:?} names nothing the loop holds, though a handle is used only until it \
                 is closed"
            )
        };
        let entry = &mut self.entries[handle.index as usize];
        entry.generation = entry.generation.wrapping_add(1);
        self.free.push(handle.index);
        held
    }

    // Tries `op`, keeping it to try again at the socket's next edge if it would block.
    fn try_op(&mut self, slot: Slot, op: Op) {
        let handle = match &op {
            Op::Accept(handle) | Op::Recv(handle, _) | Op::Send { handle, .. } => *handle,
            Op::Fsync(handle, durable) => {
                let Kind::File(file) = &self.held(*handle).kind else {
                    unreachable!("{handle:?}, synced, is no file, though `Io::fsync` takes a file")
                };
                let file = Arc::clone(file);
                return self.sync(slot, file, *durable);
            }
            Op::TryLock(handle) => {
                let Kind::File(file) = &self.held(*handle).kind else {
                    unreachable!("{handle:?}, locked, is no file, though `Io::try_lock` takes one")
                };
                let locked = match file.try_lock() {
                    Ok(()) => Ok(true),
                    Err(TryLockError::WouldBlock) => Ok(false),
                    Err(TryLockError::Error(error)) => Err(error),
                };
                return self.ready.push((slot, Outcome::Locked(locked)));
            }
            Op::Timeout(at) => return self.timeouts.push((*at, slot)),
        };
        let held = self.held(handle);
        match attempt(&mut held.kind, op) {
            Attempt::Done(outcome) => self.ready.push((slot, outcome)),
            Attempt::Accepted(accepted) => {
                let held = accepted.and_then(|stream| self.hold_socket(stream, Kind::Stream));
                self.ready.push((slot, Outcome::Accepted(held)));
            }
            Attempt::Blocked(op @ (Op::Accept(_) | Op::Recv(..))) => {
                held.reading = Some((slot, op));
            }
            Attempt::Blocked(op) => held.writing = Some((slot, op)),
        }
    }
}

// What `op` comes to when its handle closes before it completes.
fn aborted(op: Op) -> Outcome {
    let error = || {
        io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "closed before it completed",
        )
    };
    match op {
        Op::Accept(_) => Outcome::Accepted(Err(error())),
        Op::Recv(_, buffer) => Outcome::Received(buffer, Err(error())),
        Op::Send { buffer, .. } => Outcome::Sent(buffer, Err(error())),
        Op::Fsync(..) => Outcome::Synced(Err(error())),
        op @ (Op::TryLock(_) | Op::Timeout(_)) => unreachable!(
            "{op:?} was aborted with its handle, though it never waits on a socket: a lock \
             completes as it is submitted, and a timeout waits on the clock"
        ),
    }
}

enum Attempt {
    Done(Outcome),
    // A connection that the listener took, for the backend to hold.
    Accepted(io::Result<OwnedFd>),
    Blocked(Op),
}

fn attempt(kind: &mut Kind, op: Op) -> Attempt {
    let would_block = |result: &io::Result<usize>| matches!(result, Err(error) if error.kind() == io::ErrorKind::WouldBlock);
    match (kind, op) {
        (Kind::Listener(fd), Op::Accept(handle)) => {
            match rustix::net::accept_with(&*fd, SocketFlags::CLOEXEC | SocketFlags::NONBLOCK) {
                Err(rustix::io::Errno::AGAIN) => Attempt::Blocked(Op::Accept(handle)),
                accepted => Attempt::Accepted(accepted.map_err(Into::into)),
            }
        }
        (Kind::Stream(fd), Op::Recv(handle, mut buffer)) => {
            let received = loop {
                match rustix::net::recv(&*fd, &mut buffer[..], RecvFlags::empty()) {
                    Err(rustix::io::Errno::INTR) => {}
                    received => break received.map(|(read, _)| read).map_err(Into::into),
                }
            };
            if would_block(&received) {
                return Attempt::Blocked(Op::Recv(handle, buffer));
            }
            Attempt::Done(Outcome::Received(buffer, received))
        }
        (
            Kind::Stream(fd),
            Op::Send {
                handle,
                buffer,
                from,
            },
        ) => {
            let sent = loop {
                match rustix::net::send(&*fd, &buffer[from..], SendFlags::NOSIGNAL) {
                    Err(rustix::io::Errno::INTR) => {}
                    sent => break sent.map_err(Into::into),
                }
            };
            if would_block(&sent) {
                return Attempt::Blocked(Op::Send {
                    handle,
                    buffer,
                    from,
                });
            }
            Attempt::Done(Outcome::Sent(buffer, sent))
        }
        (_, op) => unreachable!(
            "{op:?} was submitted on a handle of another kind, though each of `Io`'s operations \
             takes the handle of its own kind"
        ),
    }
}

impl Backend for Epoll {
    fn now(&mut self) -> Duration {
        Machine.now()
    }

    fn listen(&mut self, path: &Path, mode: u32) -> io::Result<Handle> {
        let listener = UnixListener::bind(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
        listener.set_nonblocking(true)?;
        self.hold_socket(listener.into(), Kind::Listener)
    }

    fn adopt(&mut self, stream: OwnedFd) -> io::Result<Handle> {
        rustix::io::ioctl_fionbio(&stream, true)?;
        self.hold_socket(stream, Kind::Stream)
    }

    fn file(&mut self, file: fs::File) -> Handle {
        let index = self.vacant();
        self.hold_at(index, Kind::File(Arc::new(file)))
    }

    fn shutdown(&mut self, handle: Handle) {
        let held = self.held(handle);
        let Kind::Stream(fd) = &held.kind else {
            unreachable!("{handle:?}, shut down, is no stream, though only a stream is")
        };
        // A stream that the peer has reset is shut down already.
        let _ = rustix::net::shutdown(fd, Shutdown::Both);
        // What waits on the stream would block no longer: it completes at the edge that the
        // shutdown makes, which epoll reports as a hang-up.
    }

    fn close(&mut self, handle: Handle) {
        let Held {
            kind,
            reading,
            writing,
        } = self.release(handle);
        for (slot, op) in reading.into_iter().chain(writing) {
            self.ready.push((slot, aborted(op)));
        }
        if let Kind::Listener(fd) | Kind::Stream(fd) = &kind {
            // Which takes it off the ready list too, so no later wait reports it. Closing the
            // descriptor alone wouldn't while another descriptor shares its socket.
            let _ = epoll::delete(&self.fd, fd);
        }
    }

    fn submit(&mut self, slot: Slot, op: Op) {
        self.try_op(slot, op);
    }

    fn wait(&mut self, until: Duration, _: bool, done: &mut Vec<(Slot, Outcome)>) {
        // What completed as it was submitted doesn't wait, but epoll is asked all the same, so
        // that a socket whose operations always complete at once starves no other, as
        // TigerBeetle's epoll backend polls with no timeout in each `run`:
        // https://github.com/tigerbeetle/tigerbeetle/blob/68958eb757747d0fc96a79d1f9c2fa6766b67b48/src/stdx/io/linux_epoll.zig#L102-L124
        self.take_synced();
        let due = (self.timeouts.iter()).fold(until, |due, (at, _)| due.min(*at));
        let left = if self.ready.is_empty() {
            due.saturating_sub(self.now()).min(LOOK)
        } else {
            Duration::ZERO
        };
        let timeout = rustix::time::Timespec {
            tv_sec: 0,
            tv_nsec: i64::from(left.subsec_nanos()),
        };
        // Made for each wait, as an event's data may be a pointer, which no other thread may
        // hold, so the backend can move to its loop's thread.
        let mut events = Vec::with_capacity(EVENTS);
        let spare = rustix::buffer::spare_capacity(&mut events);
        match epoll::wait(&self.fd, spare, Some(&timeout)) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(error) => unreachable!(
                "epoll_wait failed: {error}, though its descriptor, buffer and timeout are valid"
            ),
        }
        for event in &events {
            self.ready_at(event.data.u64(), event.flags);
        }
        self.take_synced();
        // Each timeout whose time has come, as TigerBeetle's epoll backend fires them at each wait:
        // https://github.com/tigerbeetle/tigerbeetle/blob/68958eb757747d0fc96a79d1f9c2fa6766b67b48/src/stdx/io/linux_epoll.zig#L1023-L1037
        let now = self.now();
        let timed_out = self.timeouts.extract_if(.., |(at, _)| *at <= now);
        (self.ready).extend(timed_out.map(|(_, slot)| (slot, Outcome::TimedOut)));
        done.append(&mut self.ready);
    }

    fn waker(&self) -> Arc<dyn Fn() + Send + Sync> {
        let wake = Arc::clone(&self.wake);
        Arc::new(move || wake_up(&wake))
    }
}

impl Epoll {
    // Tries again what waits on the socket at `index`, which epoll reported ready.
    fn ready_at(&mut self, index: u64, flags: epoll::EventFlags) {
        if index == WAKE {
            let mut counter = [0; 8];
            // The counter resets as it is read; another wake may have read it first.
            let _ = rustix::io::read(&*self.wake, &mut counter);
            return;
        }
        let Some(Entry {
            held: Some(held), ..
        }) = usize::try_from(index)
            .ok()
            .and_then(|index| self.entries.get_mut(index))
        else {
            unreachable!(
                "epoll reported entry {index}, which holds nothing, though a close takes its \
                 socket off epoll's lists before the next wait"
            )
        };
        let failed = epoll::EventFlags::ERR | epoll::EventFlags::HUP | epoll::EventFlags::RDHUP;
        let reading = flags.intersects(epoll::EventFlags::IN | failed);
        let writing = flags.intersects(epoll::EventFlags::OUT | failed);
        let read = reading.then(|| held.reading.take()).flatten();
        let write = writing.then(|| held.writing.take()).flatten();
        for (slot, op) in read.into_iter().chain(write) {
            self.try_op(slot, op);
        }
    }
}
