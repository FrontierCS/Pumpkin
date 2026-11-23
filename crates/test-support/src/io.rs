//! The contract of [`stdx::io`]'s loop as cases through [`Io`] alone: [`outcomes`].
//!
//! Each case runs a loop of its own on a fresh backend, and its outcomes are the same on every
//! backend that keeps [`Backend`]'s contracts. So a backend's test compares them with one snapshot
//! that every backend's test shares. Each case asserts what holds of every case: each callback runs
//! in a batch after the one that submitted its operation, so never inside the call that submitted
//! it, and the case ends with nothing in flight.
//!
//! [`world_outcomes`] runs the steps that only the simulated world ([`crate::world`]) stages, on
//! one loop, with the same assertions.
// The cases follow TigerBeetle's tests of its IO, which each of its backends passes:
// https://github.com/tigerbeetle/tigerbeetle/blob/68958eb757747d0fc96a79d1f9c2fa6766b67b48/src/stdx/io/test.zig

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;
use std::{fs, mem, thread};

use stdx::io::{Backend, Durable, File, Inbox, Io, Listener, Loop, Stream};

use crate::TestDir;
use crate::world::{Script, Synced, World};

// A case puts two operations in flight at most, such as an accept and a recv.
const CAPACITY: usize = 2;

// How long a batch waits for a completion at most, and how many batches a case waits for one: a
// case that waits for what never comes fails within a second or so, under nextest's 5 s.
const WAIT: Duration = Duration::from_millis(10);
const BATCHES: usize = 100;

// The batches through which an operation that waits stays in flight: more than one, so that one
// that completes a batch late isn't taken for one that waits.
const QUIET: usize = 3;

// More than a socket's send buffer holds by default, 208 KiB on Linux (`net.core.wmem_default`),
// so a peer that doesn't read can't take it all, as in TigerBeetle's test of a pipe over a socket:
// https://github.com/tigerbeetle/tigerbeetle/blob/68958eb757747d0fc96a79d1f9c2fa6766b67b48/src/stdx/io/test.zig#L613
const MESSAGE_BYTES: usize = 1 << 20;

// What a peer sends a stream that the loop receives a little of in each batch: as many receives as
// it takes are more than a case waits for any one of, and it all fits a socket's buffer.
const BUSY_BYTES: usize = 16 << 10;
const BUSY_RECEIVE_BYTES: usize = 64;
const _: () = assert!(BUSY_BYTES / BUSY_RECEIVE_BYTES > BATCHES);

/// Runs each case on a fresh backend that `backend` makes, and lists what each came to.
///
/// `connect` connects a peer outside the loop to a path that the loop listens on. The list is the
/// same on every backend that keeps [`Backend`]'s contracts. The cases keep their files in a
/// [`TestDir`] of the calling test's.
///
/// # Panics
///
/// Panics if a callback runs in the batch that submitted it, or an operation stays in flight.
pub fn outcomes<P: Read + Write + Send + 'static>(
    backend: impl Fn() -> Box<dyn Backend>,
    connect: impl Fn(&Path) -> io::Result<P>,
) -> String {
    let dir = TestDir::new();
    let connect = |path: &Path| connect(path).unwrap();
    let cases: [(&str, Run<P>); 10] = [
        ("an accept, then a recv and a send", exchange),
        (
            "a recv that waits, beside a stream whose recvs never wait",
            beside_busy,
        ),
        ("a recv at the peer's end of the stream", end_of_stream),
        ("a shutdown with a recv in flight", shutdown),
        ("a close with an accept and a recv in flight", close),
        ("a send to a peer that doesn't read", backpressure),
        (
            "a send to a peer that reads on a thread of its own",
            to_reader,
        ),
        ("a file's sync, whose callback syncs it again", file),
        ("a next tick, whose callback submits another", next_tick),
        ("events from other threads", events),
    ];
    let mut text = String::new();
    for (index, (name, run)) in cases.into_iter().enumerate() {
        let mut case = Case::new(backend());
        let lines = run(&mut case, &dir.path().join(index.to_string()), &connect);
        case.end();
        writeln!(text, "{name}:").unwrap();
        for line in lines {
            writeln!(text, "    {line}").unwrap();
        }
    }
    text
}

// A case, given a path of its own for a socket or a file: the lines of what it came to.
type Run<P> = fn(&mut Case, &Path, &dyn Fn(&Path) -> P) -> Vec<String>;

fn connected<P>(case: &mut Case, at: &Path, connect: &dyn Fn(&Path) -> P) -> (Listener, Stream, P) {
    let listener = case.io.listen(at, 0o600).unwrap();
    let accept = case.accept(&listener);
    let peer = connect(at);
    let stream = case.wait(accept).0.stream();
    (listener, stream, peer)
}

// As TigerBeetle's test of an accept, a connect, a send and a receive:
// https://github.com/tigerbeetle/tigerbeetle/blob/68958eb757747d0fc96a79d1f9c2fa6766b67b48/src/stdx/io/test.zig#L171-L289
fn exchange<P: Read + Write>(
    case: &mut Case,
    at: &Path,
    connect: &dyn Fn(&Path) -> P,
) -> Vec<String> {
    let (listener, stream, mut peer) = connected(case, at, connect);
    peer.write_all(b"ping").unwrap();
    let recv = case.recv(&stream);
    let received = case.wait(recv).0;
    let reply = case.send(&stream, b"pong".to_vec(), 0);
    let sent = case.wait(reply).0;
    let mut read = vec![0; sent.bytes()];
    peer.read_exact(&mut read).unwrap();
    case.io.close(stream);
    case.io.close_listener(listener);
    vec![
        "accepted".to_owned(),
        format!("received {}", shown(&received)),
        format!("sent {}", shown(&sent)),
        format!("the peer read {:?}", String::from_utf8_lossy(&read)),
    ]
}

// A recv that waits completes as its peer sends, though another stream has more to receive at
// each batch: no stream starves another.
fn beside_busy<P: Read + Write>(
    case: &mut Case,
    at: &Path,
    connect: &dyn Fn(&Path) -> P,
) -> Vec<String> {
    let (listener, busy, mut busy_peer) = connected(case, at, connect);
    let accept = case.accept(&listener);
    let mut quiet_peer = connect(at);
    let quiet = case.wait(accept).0.stream();
    let waiting = case.recv(&quiet);
    let waited = case.waits(waiting);
    busy_peer.write_all(&[0; BUSY_BYTES]).unwrap();
    quiet_peer.write_all(b"hi").unwrap();
    let mut busy_bytes = 0;
    let received = loop {
        if let Some((received, _)) = case.owner.done.remove(&waiting) {
            break received;
        }
        assert!(
            busy_bytes < BUSY_BYTES,
            "the recv that waits is still in flight once the other stream has received all: \
             expected it to complete as its peer sends"
        );
        let token = case.owner.token();
        case.io
            .recv(&busy, vec![0; BUSY_RECEIVE_BYTES], token, on_bytes);
        busy_bytes += case.wait(token).0.bytes();
    };
    let left = if busy_bytes < BUSY_BYTES {
        "had more to receive"
    } else {
        "had received all"
    };
    case.io.close(busy);
    case.io.close(quiet);
    case.io.close_listener(listener);
    drop((busy_peer, quiet_peer));
    vec![
        format!(
            "it {} while its peer sends nothing",
            if waited { "waits" } else { "completes" }
        ),
        format!(
            "once its peer sends, it received {}, as the other stream {left}",
            shown(&received)
        ),
    ]
}

fn end_of_stream<P: Read + Write>(
    case: &mut Case,
    at: &Path,
    connect: &dyn Fn(&Path) -> P,
) -> Vec<String> {
    let (listener, stream, mut peer) = connected(case, at, connect);
    peer.write_all(b"bye").unwrap();
    drop(peer);
    let recv = case.recv(&stream);
    let first = case.wait(recv).0;
    let recv = case.recv(&stream);
    let then = case.wait(recv).0;
    case.io.close(stream);
    case.io.close_listener(listener);
    vec![format!("received {}, then {}", shown(&first), shown(&then))]
}

fn shutdown<P: Read + Write>(
    case: &mut Case,
    at: &Path,
    connect: &dyn Fn(&Path) -> P,
) -> Vec<String> {
    let (listener, stream, mut peer) = connected(case, at, connect);
    let recv = case.recv(&stream);
    case.io.shutdown(&stream);
    let in_flight = case.wait(recv).0;
    let recv = case.recv(&stream);
    let later = case.wait(recv).0;
    let peer_read = peer.read(&mut [0; 8]).unwrap();
    case.io.close(stream);
    case.io.close_listener(listener);
    vec![
        format!("the recv in flight received {}", shown(&in_flight)),
        format!("a later recv received {}", shown(&later)),
        format!("the peer read {peer_read} bytes"),
    ]
}

fn close<P: Read + Write>(case: &mut Case, at: &Path, connect: &dyn Fn(&Path) -> P) -> Vec<String> {
    let (listener, stream, peer) = connected(case, at, connect);
    let accept = case.accept(&listener);
    let recv = case.recv(&stream);
    case.io.close_listener(listener);
    case.io.close(stream);
    let accepted = case.wait(accept).0;
    let received = case.wait(recv).0;
    drop(peer);
    vec![
        format!("the accept {}", shown(&accepted)),
        format!("the recv {}", shown(&received)),
    ]
}

// As TigerBeetle's test of a pipe over a socket, which sends until all has gone:
// https://github.com/tigerbeetle/tigerbeetle/blob/68958eb757747d0fc96a79d1f9c2fa6766b67b48/src/stdx/io/test.zig#L606-L767
fn backpressure<P: Read + Write>(
    case: &mut Case,
    at: &Path,
    connect: &dyn Fn(&Path) -> P,
) -> Vec<String> {
    let (listener, stream, mut peer) = connected(case, at, connect);
    // Of a prime length, so that a send from the wrong place shows in what the peer reads.
    let message: Vec<u8> = (0..251).cycle().take(MESSAGE_BYTES).collect();
    let first = case.send(&stream, message.clone(), 0);
    let (buffer, sent_bytes) = case.wait(first).0.sent();
    let first = if sent_bytes < MESSAGE_BYTES {
        "a part"
    } else {
        "all"
    };
    let sending = case.send(&stream, buffer, sent_bytes);
    let rest = if case.waits(sending) { "waits" } else { "goes" };
    let mut read = Vec::with_capacity(MESSAGE_BYTES);
    case.send_rest(&stream, (sending, sent_bytes), |sent_bytes| {
        drain(&mut peer, &mut read, sent_bytes);
    });
    drain(&mut peer, &mut read, MESSAGE_BYTES);
    case.io.close(stream);
    case.io.close_listener(listener);
    vec![
        format!("the first send of {MESSAGE_BYTES} bytes sent {first} of them"),
        format!("the rest {rest} while the peer doesn't read"),
        format!(
            "once it reads, the rest goes, and the peer reads every byte {}",
            order(&read, &message)
        ),
    ]
}

// The peer's read on a thread of its own waits for the loop's send, which waits for the read, so
// each wakes the other: the last send too, after which the loop submits nothing.
fn to_reader<P: Read + Write + Send + 'static>(
    case: &mut Case,
    at: &Path,
    connect: &dyn Fn(&Path) -> P,
) -> Vec<String> {
    let (listener, stream, mut peer) = connected(case, at, connect);
    let message: Vec<u8> = (0..251).cycle().take(MESSAGE_BYTES).collect();
    let reader = thread::spawn(move || {
        let mut read = vec![0; MESSAGE_BYTES];
        peer.read_exact(&mut read).unwrap();
        read
    });
    let sending = case.send(&stream, message.clone(), 0);
    case.send_rest(&stream, (sending, 0), |_| {});
    let mut batches = 0;
    while !reader.is_finished() {
        assert!(
            batches < BATCHES,
            "the peer is still reading {BATCHES} batches after the loop sent all: expected it to \
             read what the loop sent"
        );
        batches += 1;
        case.run();
    }
    let read = reader.join().unwrap();
    case.io.close(stream);
    case.io.close_listener(listener);
    vec![format!(
        "the peer reads every byte {}",
        order(&read, &message)
    )]
}

fn order(read: &[u8], sent: &[u8]) -> &'static str {
    if read == sent {
        "as sent"
    } else {
        "out of order"
    }
}

// Reads from `peer` until `read` holds `sent` bytes, never more than the loop has sent it, so
// that a read never waits for the loop.
fn drain(peer: &mut impl Read, read: &mut Vec<u8>, sent: usize) {
    let mut chunk = vec![0; 64 << 10];
    while read.len() < sent {
        let most = chunk.len().min(sent - read.len());
        let bytes = peer.read(&mut chunk[..most]).unwrap();
        assert!(
            bytes > 0,
            "the peer read the end of the stream: expected the bytes that the loop sent"
        );
        read.extend_from_slice(&chunk[..bytes]);
    }
}

// As TigerBeetle's test of a file's operations, each submitted by the callback of the one before:
// https://github.com/tigerbeetle/tigerbeetle/blob/68958eb757747d0fc96a79d1f9c2fa6766b67b48/src/stdx/io/test.zig#L97-L131
fn file<P>(case: &mut Case, at: &Path, _: &dyn Fn(&Path) -> P) -> Vec<String> {
    let mut written = fs::File::create(at).unwrap();
    written.write_all(b"hello").unwrap();
    let file = case.io.file(written);
    let sync = case.owner.token();
    case.io.fsync(&file, Durable::Data, sync, on_synced_again);
    case.owner.file = Some(file);
    let synced = case.wait(sync).0;
    let again = case.owner.chained.take().unwrap();
    let again = case.wait(again).0;
    case.io.close_file(case.owner.file.take().unwrap());
    vec![
        format!("its sync of the data {}", shown(&synced)),
        format!(
            "the sync of all that its callback submitted {}",
            shown(&again)
        ),
        format!("the file holds {:?}", fs::read_to_string(at).unwrap()),
    ]
}

fn next_tick<P>(case: &mut Case, _: &Path, _: &dyn Fn(&Path) -> P) -> Vec<String> {
    let tick = case.owner.token();
    case.io.next_tick(tick, on_tick_again);
    let (ticked, ticked_in) = case.wait(tick);
    let again = case.owner.chained.take().unwrap();
    let (again, again_in) = case.wait(again);
    vec![
        format!("{} in batch {ticked_in}", shown(&ticked)),
        format!(
            "the tick that it submitted {} in batch {again_in}",
            shown(&again)
        ),
    ]
}

// As TigerBeetle's test of an event that another thread triggers:
// https://github.com/tigerbeetle/tigerbeetle/blob/68958eb757747d0fc96a79d1f9c2fa6766b67b48/src/stdx/io/test.zig#L354-L417
fn events<P>(case: &mut Case, _: &Path, _: &dyn Fn(&Path) -> P) -> Vec<String> {
    let events = case.io.events();
    thread::spawn(move || events.send(7)).join().unwrap();
    let sent = case.batch();
    let edge = case.io.edge();
    thread::spawn(move || edge.send(8)).join().unwrap();
    let edge_sent = case.batch();
    let edge = case.io.edge();
    thread::spawn(move || drop(edge)).join().unwrap();
    let dropped = case.batch();
    vec![
        format!("7 sent through `Events`: {sent}"),
        format!("8 sent through an `Edge`: {edge_sent}"),
        format!("an `Edge` dropped unsent: {dropped}"),
    ]
}

// The files that the world's steps sync, in the order of their scripts.
const SYNCED: [&str; 5] = ["failed", "late", "owed", "counted", "held"];

/// Runs the steps that only a simulated world stages, and lists what each came to.
///
/// The steps run on one loop in a fresh [`World`]. They pin the clock's stops, the scripts, what
/// counts as an event, the peers' moves and shutdowns, and the backend's drop.
///
/// # Panics
///
/// Panics if a callback runs in the batch that submitted it, or an operation stays in flight.
#[expect(
    clippy::too_many_lines,
    reason = "one world's steps, each a line of what it lists"
)]
pub fn world_outcomes() -> String {
    let dir = TestDir::new();
    let world = World::new();
    let secs = Duration::from_secs;
    let file = |name: &'static str| Box::new(move |path: &Path| path.ends_with(name));
    let lasts = |at_secs, events| Synced::Lasts {
        at: secs(at_secs),
        events,
    };
    let done = dir.path().join("done");
    let synced = [
        Synced::Fails,
        lasts(10, 0),
        lasts(12, 0),
        lasts(0, 4),
        Synced::Held(Box::new(move |_| done.exists())),
    ];
    for (name, synced) in SYNCED.into_iter().zip(synced) {
        world.script(Script {
            matches: file(name),
            synced,
        });
    }
    let mut case = Case::new(world.backend());
    let files = SYNCED.map(|name| {
        case.io
            .file(fs::File::create(dir.path().join(name)).unwrap())
    });
    let sync = |case: &mut Case, name: &str| {
        let index = SYNCED.iter().position(|synced| *synced == name).unwrap();
        let token = case.fsync(&files[index]);
        case.names.insert(token, format!("the sync of {name}"));
    };
    let socket = dir.path().join("socket");
    let listener = case.io.listen(&socket, 0o600).unwrap();
    let accept = case.accept(&listener);
    let mut peer = world.connect(&socket).unwrap();
    let stream = case.wait(accept).0.stream();
    let recv = |case: &mut Case| {
        let token = case.recv(&stream);
        case.names.insert(token, "the recv of".to_owned());
    };
    recv(&mut case);
    let mut waiting = peer.try_clone().unwrap();
    let clock = world.clone();
    let waiter = thread::spawn(move || {
        clock.wait_until(secs(10));
        waiting.write_all(b"after 10s").unwrap();
    });

    let mut text = String::new();
    // Runs batches as `Case::ran` does, and lists what they ran.
    let mut step = |case: &mut Case, what: &str, until_secs: u64, most: usize| {
        writeln!(text, "{what}: {}", case.ran(secs(until_secs), most)).unwrap();
    };
    sync(&mut case, "failed");
    step(&mut case, "a sync that fails", 1, 1);
    sync(&mut case, "late");
    step(&mut case, "a sync due at 10s, in a wait until 4s", 4, 1);
    step(&mut case, "then in a wait until 20s", 20, 1);
    step(&mut case, "a write that waited until 10s", 20, BATCHES);
    waiter.join().unwrap();

    let edge = case.io.edge();
    sync(&mut case, "owed");
    step(&mut case, "a sync due at 12s, with an event owed", 20, 1);
    edge.send(1);
    step(&mut case, "the edge's event", 20, BATCHES);
    step(&mut case, "then", 20, 1);

    // Four events since the sync before it began: the edge's, a write, the peer's end, and 2.
    sync(&mut case, "counted");
    recv(&mut case);
    peer.move_on_write(secs(30));
    peer.write_all(b"moved").unwrap();
    step(&mut case, "a write that moves time to 30s", 40, BATCHES);
    peer.shutdown_read();
    let send = case.send(&stream, b"lost".to_vec(), 0);
    case.names.insert(send, "the send".to_owned());
    step(&mut case, "a send once the peer reads no more", 40, BATCHES);
    recv(&mut case);
    peer.shutdown();
    drop(peer);
    step(&mut case, "the peer's shutdown, then its drop", 40, BATCHES);
    case.io.events().send(2);
    step(&mut case, "an event", 40, BATCHES);

    sync(&mut case, "held");
    world.reach(secs(50));
    step(&mut case, "a held sync, once the world reached 50s", 60, 1);
    fs::write(dir.path().join("done"), "").unwrap();
    step(&mut case, "once its function says so", 60, BATCHES);

    let accept = case.accept(&listener);
    let mut kept = world.connect(&socket).unwrap();
    // Left to the backend's drop, which closes what it holds.
    let _accepted = case.wait(accept).0.stream();
    case.io.close_listener(listener);
    case.io.close(stream);
    case.end();
    let read = match kept.read(&mut [0; 8]) {
        Ok(bytes) => format!("its peer read {bytes} bytes"),
        Err(error) => format!("its peer's read failed: {:?}", error.kind()),
    };
    writeln!(
        text,
        "a stream that the loop held as its backend dropped: {read}"
    )
    .unwrap();
    text
}

fn shown(done: &Done) -> String {
    match done {
        Done::Accepted(Ok(_)) => "accepted".to_owned(),
        Done::Bytes(_, Ok(0)) => "0 bytes".to_owned(),
        Done::Bytes(buffer, Ok(bytes)) => {
            format!("{:?}", String::from_utf8_lossy(&buffer[..*bytes]))
        }
        Done::Synced(Ok(())) => "completed".to_owned(),
        Done::Ticked => "ran".to_owned(),
        Done::Accepted(Err(error)) | Done::Bytes(_, Err(error)) | Done::Synced(Err(error)) => {
            format!("failed: {:?}", error.kind())
        }
    }
}

struct Case {
    io: Io<Owner>,
    owner: Owner,
    // The names of operations that `Case::ran` shows, by token.
    names: BTreeMap<u64, String>,
}

impl Case {
    fn new(backend: Box<dyn Backend>) -> Self {
        let (_, inbox) = Inbox::new(&*backend);
        Self {
            io: Io::new(backend, CAPACITY, inbox),
            owner: Owner::default(),
            names: BTreeMap::new(),
        }
    }

    fn run(&mut self) {
        let until = self.io.now() + WAIT;
        self.run_until(until);
    }

    fn run_until(&mut self, until: Duration) {
        self.owner.batches_count += 1;
        self.io.run_until(&mut self.owner, until);
    }

    // Runs batches that wait until `until`, `most` at most, up to the first that completes an
    // operation or takes an event: the time it read, and what it ran, each operation by its name.
    fn ran(&mut self, until: Duration, most: usize) -> String {
        for _ in 0..most {
            if !self.owner.done.is_empty() || !self.owner.events.is_empty() {
                break;
            }
            self.run_until(until);
        }
        let done = mem::take(&mut self.owner.done).into_iter();
        let mut ran: Vec<String> = done
            .map(|(token, (done, _))| format!("{} {}", self.names[&token], shown(&done)))
            .collect();
        let events = self.owner.events.drain(..);
        ran.extend(events.map(|event| format!("took {event}")));
        let ran = if ran.is_empty() {
            "nothing".to_owned()
        } else {
            ran.join(", ")
        };
        format!("at {}s, {ran}", self.io.now().as_secs_f64())
    }

    // Runs a batch, and says what `Loop::event` took in it.
    fn batch(&mut self) -> String {
        self.run();
        let events: Vec<String> = (self.owner.events.drain(..))
            .map(|event| event.to_string())
            .collect();
        let took = if events.is_empty() {
            "nothing".to_owned()
        } else {
            events.join(", ")
        };
        let batch = self.owner.batches_count;
        format!("batch {batch} took {took}")
    }

    // Runs batches until `token`'s operation completes: its outcome, and the batch it completed in.
    fn wait(&mut self, token: u64) -> (Done, u64) {
        let mut batches = 0;
        loop {
            if let Some((done, batch)) = self.owner.done.remove(&token) {
                return (done, batch);
            }
            assert!(
                batches < BATCHES,
                "operation {token} is still in flight after {BATCHES} batches: expected it to \
                 complete at once"
            );
            batches += 1;
            self.run();
        }
    }

    // Whether `token`'s operation is still in flight after a few batches.
    fn waits(&mut self, token: u64) -> bool {
        for _ in 0..QUIET {
            self.run();
        }
        self.owner.in_flight.contains_key(&token)
    }

    fn accept(&mut self, listener: &Listener) -> u64 {
        let token = self.owner.token();
        self.io.accept(listener, token, on_accept);
        token
    }

    fn recv(&mut self, stream: &Stream) -> u64 {
        let token = self.owner.token();
        self.io.recv(stream, vec![0; 64], token, on_bytes);
        token
    }

    fn send(&mut self, stream: &Stream, buffer: Vec<u8>, from: usize) -> u64 {
        let token = self.owner.token();
        self.io.send(stream, (buffer, from), token, on_bytes);
        token
    }

    fn fsync(&mut self, file: &File) -> u64 {
        let token = self.owner.token();
        self.io.fsync(file, Durable::Data, token, on_synced);
        token
    }

    // Waits for `sending`, which sends a message of `MESSAGE_BYTES` from `sent_bytes`, and sends
    // what is left until all has gone, calling `before` with the bytes gone before each wait.
    fn send_rest(
        &mut self,
        stream: &Stream,
        (mut sending, mut sent_bytes): (u64, usize),
        mut before: impl FnMut(usize),
    ) {
        loop {
            before(sent_bytes);
            let (buffer, more_bytes) = self.wait(sending).0.sent();
            sent_bytes += more_bytes;
            if sent_bytes == MESSAGE_BYTES {
                return;
            }
            assert!(
                more_bytes > 0,
                "a send of {} bytes sent none: expected it to wait until it can send some",
                MESSAGE_BYTES - sent_bytes
            );
            sending = self.send(stream, buffer, sent_bytes);
        }
    }

    fn end(self) {
        let Owner {
            in_flight,
            done,
            events,
            ..
        } = &self.owner;
        assert!(
            in_flight.is_empty(),
            "the case ended with operations {in_flight:?} in flight: expected it to wait for each"
        );
        assert!(
            done.is_empty() && events.is_empty(),
            "the case left outcomes {done:?} and events {events:?}: expected it to show each"
        );
    }
}

#[derive(Default)]
struct Owner {
    // The batches that the loop has run, so the number of the one that runs now, counted from 1.
    batches_count: u64,
    tokens_count: u64,
    // Each operation in flight, by token, with the batch that submitted it, 0 before the first.
    in_flight: BTreeMap<u64, u64>,
    // Each operation's outcome, by token, with the batch that it completed in.
    done: BTreeMap<u64, (Done, u64)>,
    events: Vec<u64>,
    // The file that a sync's callback syncs again.
    file: Option<File>,
    // The operation that a callback submitted, for its case to wait on.
    chained: Option<u64>,
}

#[derive(Debug)]
enum Done {
    Accepted(io::Result<Stream>),
    Bytes(Vec<u8>, io::Result<usize>),
    Synced(io::Result<()>),
    Ticked,
}

impl Done {
    fn stream(self) -> Stream {
        match self {
            Self::Accepted(Ok(stream)) => stream,
            done => panic!("{done:?}: expected an accepted stream"),
        }
    }

    fn sent(self) -> (Vec<u8>, usize) {
        match self {
            Self::Bytes(buffer, Ok(bytes)) => (buffer, bytes),
            done => panic!("{done:?}: expected bytes sent"),
        }
    }

    fn bytes(&self) -> usize {
        match self {
            Self::Bytes(_, Ok(bytes)) => *bytes,
            done => panic!("{done:?}: expected bytes sent"),
        }
    }
}

impl Loop for Owner {
    type Event = u64;

    fn event(&mut self, _: &mut Io<Self>, event: u64) {
        self.events.push(event);
    }
}

impl Owner {
    // A token for an operation submitted now, which is in flight from this batch on.
    fn token(&mut self) -> u64 {
        self.tokens_count += 1;
        self.in_flight.insert(self.tokens_count, self.batches_count);
        self.tokens_count
    }

    fn complete(&mut self, token: u64, done: Done) {
        let Some(submitted) = self.in_flight.remove(&token) else {
            panic!("operation {token} completed with {done:?}, though nothing of it was in flight")
        };
        let batch = self.batches_count;
        assert!(
            batch > submitted,
            "operation {token}, submitted in batch {submitted}, completed in it: expected it to \
             complete in a later batch"
        );
        self.done.insert(token, (done, batch));
    }
}

fn on_accept(owner: &mut Owner, _: &mut Io<Owner>, token: u64, accepted: io::Result<Stream>) {
    owner.complete(token, Done::Accepted(accepted));
}

fn on_bytes(
    owner: &mut Owner,
    _: &mut Io<Owner>,
    token: u64,
    buffer: Vec<u8>,
    result: io::Result<usize>,
) {
    owner.complete(token, Done::Bytes(buffer, result));
}

fn on_synced(owner: &mut Owner, _: &mut Io<Owner>, token: u64, result: io::Result<()>) {
    owner.complete(token, Done::Synced(result));
}

fn on_tick(owner: &mut Owner, _: &mut Io<Owner>, token: u64) {
    owner.complete(token, Done::Ticked);
}

fn on_synced_again(owner: &mut Owner, io: &mut Io<Owner>, token: u64, result: io::Result<()>) {
    owner.complete(token, Done::Synced(result));
    let again = owner.token();
    owner.chained = Some(again);
    io.fsync(owner.file.as_ref().unwrap(), Durable::All, again, on_synced);
}

fn on_tick_again(owner: &mut Owner, io: &mut Io<Owner>, token: u64) {
    owner.complete(token, Done::Ticked);
    let again = owner.token();
    owner.chained = Some(again);
    io.next_tick(again, on_tick);
}
