//! Ops: calls that callers in the sandbox make through `sandboxctl`, and that `sandboxd` carries
//! over the session to its owner.
//!
//! `sandboxd` serves them on the control socket, in a directory it makes, which only the
//! sandbox's user may enter. A caller connects and sends one frame: its message is the
//! `OpCall`, and its body, for a function's call or a submission, the absolute path of the
//! payload's file, which `sandboxd` reads itself, since both run as the same user, and opens
//! through no symbolic link; any other call's body is ignored. The caller keeps the connection
//! open until one frame comes back, a `Result<OpReply, String>`: the reply, or why the op couldn't
//! be carried. Callers take turns. The connection closes unanswered if the session ends first, as
//! `sandboxd` then exits, or if `sandboxd` can't start a thread for the caller or write its
//! answer.
//!
//! A caller that connects and sends nothing holds a thread, with no timeout, so thousands can
//! use up the sandbox's process limit, which harms only the sandbox's own run, as a fork bomb
//! in it would.

use std::ffi::OsStr;
use std::fs::{self, DirBuilder, OpenOptions, Permissions};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use schema::sandbox::frame::{self, MAX_BODY_BYTES};
use schema::sandbox::op::{OpCall, OpReply};
use schema::sandbox::{CONTROL_SOCKET, FromSandbox, MAX_OP_PAYLOAD_BYTES};
use stdx::{Context, unpoisoned};

use crate::session::Sender;
use crate::sys;

#[derive(Clone, Default)]
/// The op in flight and the callers' turns, which the threads that carry ops and the session,
/// which receives their replies, share.
pub(crate) struct Ops(Arc<Shared>);

#[derive(Default)]
// The threads that carry callers' ops wait their turns on `turn_ended`, and the session's
// thread takes the op in flight out of `flight` to hand over its reply.
struct Shared {
    flight: Mutex<Flight>,
    turn_ended: Condvar,
}

#[derive(Default)]
struct Flight {
    next: u32,
    /// The op in flight, by number, and where its reply goes: from the session's thread to the
    /// thread that carries the op.
    waiting: Option<(u32, SyncSender<OpReply>)>,
    // Callers take turns as at a ticket counter: each takes `next_turn` as it arrives, and
    // carries its op once `turn` reaches it.
    next_turn: u64,
    turn: u64,
}

/// A caller's turn to carry its op, which passes to the next caller when it drops.
struct Turn<'a>(&'a Ops);

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        let mut flight = self.0.lock();
        flight.turn = flight.turn.wrapping_add(1);
        drop(flight);
        self.0.0.turn_ended.notify_all();
    }
}

/// Why an op got no reply from the gateway.
enum Unanswered {
    /// `sandboxd` couldn't carry it, for this reason, which the caller is told.
    Failed(String),
    /// Its caller closed the connection before the op could be sent, so it never was.
    Left,
    /// The session ended first.
    Ended,
}

impl Ops {
    /// Serves callers at the control socket, on a thread of its own. Its directory, which must not
    /// exist yet, is made closed to all but the sandbox's user, so that nothing else is there.
    pub(crate) fn serve(&self, sender: Sender) -> Result<(), String> {
        let socket = Path::new(CONTROL_SOCKET);
        let Some(dir) = socket.parent() else {
            unreachable!(
                "the control socket {CONTROL_SOCKET} has no directory, though it is an absolute \
                 path to a file in one"
            )
        };
        // Made with no more than the mode, then set to it, since a umask may take bits from the
        // first but not the second.
        DirBuilder::new()
            .mode(0o700)
            .create(dir)
            .and_then(|()| fs::set_permissions(dir, Permissions::from_mode(0o700)))
            .map_err(|error| {
                format!(
                    "the control socket's directory {} can't be made: {error}: expected nothing \
                     there yet, in a directory this user can write to",
                    dir.display()
                )
            })?;
        let listener = UnixListener::bind(socket).context(CONTROL_SOCKET)?;
        let ops = self.clone();
        thread::Builder::new()
            // Accepts callers while the session's thread serves the gateway.
            .name("ops".to_owned())
            .spawn(move || ops.accept(&listener, &sender))
            .context("the thread that accepts callers didn't start")?;
        Ok(())
    }

    fn accept(&self, listener: &UnixListener, sender: &Sender) {
        for connection in listener.incoming() {
            let Ok(connection) = connection else {
                // Out of descriptors, say: wait for callers to finish rather than spin.
                thread::sleep(Duration::from_millis(10));
                continue;
            };
            let (ops, sender) = (self.clone(), sender.clone());
            let _ = thread::Builder::new()
                // A thread per caller, which blocks while the caller waits its turn and its op's
                // reply. A caller that can't get a thread sees its connection close.
                .name("op".to_owned())
                .spawn(move || ops.carry(connection, &sender));
        }
    }

    /// Hands the gateway's reply to op `op` to the caller waiting for it. An op that is not in
    /// flight breaks the protocol, and the error ends the session.
    pub(crate) fn reply(&self, op: u32, reply: OpReply) -> io::Result<()> {
        let in_flight = self.lock().waiting.take_if(|(number, _)| *number == op);
        let Some((_, caller)) = in_flight else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "the gateway broke the protocol: it answered op {op}, which is not in flight"
                ),
            ));
        };
        match caller.try_send(reply) {
            // A caller stops waiting for its op's reply only when sending the op failed, which
            // leaves part of the op unsent and the session ending. So only a gateway that answers
            // an op before it is whole gets here, with nobody left to tell.
            Ok(()) | Err(TrySendError::Disconnected(_)) => Ok(()),
            // A one-shot channel would take a dependency or one written here, as std's
            // (`sync::oneshot`) is unstable.
            Err(TrySendError::Full(_)) => unreachable!(
                "op {op} was answered twice, though its channel holds one reply, and only the \
                 call that takes the op out of flight, under the lock, sends one"
            ),
        }
    }

    fn carry(&self, mut connection: UnixStream, sender: &Sender) {
        let answer = match frame::read::<OpCall>(&mut connection) {
            Ok(Some((call, path))) => self.call(call, &path, &connection, sender),
            Ok(None) => return,
            Err(error) => Err(error)
                .context("the request is not an op")
                .map_err(Unanswered::Failed),
        };
        let answer = match answer {
            Ok(reply) => Ok(reply),
            Err(Unanswered::Failed(message)) => Err(message),
            Err(Unanswered::Left | Unanswered::Ended) => return,
        };
        // A caller that has gone has nobody to tell.
        let _ = frame::write(&mut connection, &answer, &[]);
    }

    /// Carries one op, whose request has `body`, for the caller on `connection`, in the caller's
    /// turn, which passes to the next caller once the gateway has answered, or at once if the op
    /// failed or was dropped: so one op is in flight, and one payload held, at most.
    fn call(
        &self,
        call: OpCall,
        body: &[u8],
        connection: &UnixStream,
        sender: &Sender,
    ) -> Result<OpReply, Unanswered> {
        let path = (call.takes_payload())
            .then(|| payload_path(body))
            .transpose()?;
        // Read in the caller's turn, and before the op opens, since an open op can't be withdrawn:
        // waiting callers hold no payload, and a file that can't be read, or one over
        // `MAX_OP_PAYLOAD_BYTES`, fails this caller alone.
        let _turn = self.turn();
        let payload = path
            .map_or(Ok(Vec::new()), read_payload)
            .map_err(Unanswered::Failed)?;
        // A caller gone by its turn, as one killed while it waited, is dropped unsent, so that a
        // call given up on costs nothing; closing only its writing side counts as gone. Once sent,
        // the op is carried out even if its caller leaves.
        if sys::peer_closed(connection) {
            return Err(Unanswered::Left);
        }
        let (op, reply) = self.open();
        // A failed send means the session has failed, and is ending.
        send(sender, op, call, &payload).map_err(|_| Unanswered::Ended)?;
        drop(payload);
        let Ok(reply) = reply.recv() else {
            unreachable!(
                "op {op} lost where its reply goes without a reply, though its sender stays in \
                 the flight until `Ops::reply` takes it, and sends the reply before dropping it, \
                 or a later turn's `open` replaces it, which can't happen while this caller \
                 holds its turn; when the session ends, `sandboxd` exits with the sender still \
                 there"
            )
        };
        Ok(reply)
    }

    /// Waits for the caller's turn, which comes once each caller that arrived before it has had
    /// its own.
    fn turn(&self) -> Turn<'_> {
        let mut flight = self.lock();
        let turn = flight.next_turn;
        flight.next_turn = turn.wrapping_add(1);
        while flight.turn != turn {
            flight = unpoisoned(self.0.turn_ended.wait(flight));
        }
        Turn(self)
    }

    /// Puts a new op in flight, in the caller's turn, and returns its number and where its reply
    /// will arrive. An op left in flight by an earlier turn whose session failed gets no reply, so
    /// this one replaces it.
    fn open(&self) -> (u32, Receiver<OpReply>) {
        let mut flight = self.lock();
        let op = flight.next;
        flight.next = op.wrapping_add(1);
        let (caller, reply) = mpsc::sync_channel(1);
        flight.waiting = Some((op, caller));
        (op, reply)
    }

    fn lock(&self) -> MutexGuard<'_, Flight> {
        unpoisoned(self.0.flight.lock())
    }
}

fn payload_path(body: &[u8]) -> Result<&Path, Unanswered> {
    let path = Path::new(OsStr::from_bytes(body));
    // A relative path would be read from `sandboxd`'s working directory, not the caller's.
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(Unanswered::Failed(format!(
            "`{}` is not an absolute path: send the payload file's absolute path",
            path.display()
        )))
    }
}

/// Reads the payload in the file at `path`, which must be a regular file of at most
/// `MAX_OP_PAYLOAD_BYTES`. `sys::open_regular` opens it, so its type is checked before anything
/// is read, and a read that would block fails.
fn read_payload(path: &Path) -> Result<Vec<u8>, String> {
    let too_long = || {
        format!(
            "{} holds more than {MAX_OP_PAYLOAD_BYTES} bytes: send at most that",
            path.display()
        )
    };
    let file = sys::open_regular(path, OpenOptions::new().read(true))?;
    let size = file.metadata().context(path.display())?.len();
    if size > MAX_OP_PAYLOAD_BYTES {
        return Err(too_long());
    }
    // A byte more than the file holds, so that reading it whole takes no second allocation.
    let expected = size + 1;
    let Ok(capacity) = usize::try_from(expected) else {
        unreachable!(
            "a capacity of {expected} bytes overflows a usize, though it is at most \
             `MAX_OP_PAYLOAD_BYTES` and a byte, 64 MiB and one, which fits the usize of every \
             Linux target, of 32 bits or more"
        )
    };
    let mut payload = Vec::with_capacity(capacity);
    // A file that grew since its size was read is read to one byte over the limit, and refused.
    file.take(MAX_OP_PAYLOAD_BYTES + 1)
        .read_to_end(&mut payload)
        .context(path.display())?;
    if payload.len() as u64 > MAX_OP_PAYLOAD_BYTES {
        return Err(too_long());
    }
    Ok(payload)
}

fn send(sender: &Sender, op: u32, call: OpCall, payload: &[u8]) -> io::Result<()> {
    let size = payload.len() as u64;
    sender.send(&FromSandbox::Op { op, call, size }, &[])?;
    for chunk in payload.chunks(MAX_BODY_BYTES) {
        sender.send(&FromSandbox::OpData { op }, chunk)?;
    }
    Ok(())
}
