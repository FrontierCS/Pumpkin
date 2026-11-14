//! Where a sandbox meets the gateway, and the hello that opens its session there.
//!
//! A sandbox meets the gateway where its [`Registration`] says: at a socket of its own, which
//! only it can reach, in its run's [`Sockets`]. The registration accepts the [`Session`] the
//! sandbox opens with its hello.

use std::fs::{self, DirBuilder, File};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;
use std::{fmt, thread};

use schema::ir::{GpuModel, Name};
use schema::sandbox::{Hello, HelloReply, OpPayloadLimit, SOCKET_NAME, SocketDial, frame};
use stdx::Deadline;

use crate::{Arrival, Connection, Session, Until, context, passed};

/// The directory that names each file this process holds open by its descriptor's number,
/// through which a path resolves as through the file, a directory included.
const OPEN_FILES: &str = "/proc/self/fd";

/// How long a wait sleeps between its looks: a registration's accept for its sandbox, and a
/// hello's read for its bytes.
const POLL: Duration = Duration::from_millis(10);

/// The directory of a run's sockets, which only this user may enter, where each sandbox gets a
/// socket of its own, which only that sandbox can reach. Dropping it removes the directory and
/// all it holds.
#[derive(Debug)]
pub struct Sockets {
    dir: PathBuf,
}

impl Sockets {
    /// Makes `dir`, which must not exist yet, in a directory where no other user may rename
    /// entries: one who could would swap `dir` for a directory of their own, where the sockets
    /// would then be issued. Its path may be of any length, as each socket is bound through its
    /// directory's handle in `/proc/self/fd`.
    ///
    /// # Errors
    ///
    /// Fails if the directory can't be made.
    pub fn create(dir: PathBuf) -> io::Result<Self> {
        DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|error| {
                let what = format_args!("the sockets' directory {} can't be made", dir.display());
                context(what, &error)
            })?;
        Ok(Self { dir })
    }

    /// Issues a sandbox its socket, in a new directory `name` within this one, where the sandbox
    /// dials, as [`SocketDial`] says, and its registration accepts it. The socket takes the
    /// first connection, whoever makes it, which identifies the sandbox, so show the directory to
    /// that sandbox alone.
    ///
    /// # Errors
    ///
    /// Fails if the directory or the socket can't be made.
    pub fn issue(&self, name: &Name) -> io::Result<(SocketDial, Registration)> {
        // A name is one path component, neither `.` nor `..`, so each sandbox's directory is
        // directly within this one: none is outside it, and none holds another's.
        let dir = self.dir.join(name.as_str());
        fs::create_dir(&dir).map_err(|error| context(dir.display(), &error))?;
        // Set, not left to the umask, which may take the traverse away. The sandbox's user isn't
        // this one, and may share its group: traversing is all it needs, to reach the socket it
        // knows by name. The run's directory, 0700, keeps every other local user out, and the
        // provider shows this one to its sandbox alone.
        let traverse = fs::Permissions::from_mode(0o711);
        fs::set_permissions(&dir, traverse).map_err(|error| {
            let what = format_args!("the mode of {} can't be set", dir.display());
            context(what, &error)
        })?;
        let listener = Listener::bind(&dir)?;
        let registration = Registration {
            meeting: Meeting::Socket(listener),
        };
        Ok((SocketDial { dir }, registration))
    }
}

impl Drop for Sockets {
    fn drop(&mut self) {
        // Each socket has gone with its registration by now, so this removes empty directories,
        // which hold nothing a run needs: a failure to remove them is left unreported.
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A sandbox's connection, once its hello has been read, and the GPUs that the hello names.
#[derive(Debug)]
struct Met {
    connection: Connection,
    gpus: Option<Vec<GpuModel>>,
}

/// Where one sandbox meets the gateway, and the accept of the session it opens there.
///
/// [`Sockets::issue`] issues it. The sandbox meets the gateway as it connects to its socket.
/// Dropping the registration ends the meeting: it removes the socket.
#[derive(Debug)]
pub struct Registration {
    meeting: Meeting,
}

/// Where a [`Registration`]'s sandbox meets the gateway.
#[derive(Debug)]
enum Meeting {
    Socket(Listener),
}

impl Registration {
    /// Waits until `by`, or until `stopped` returns true, for the sandbox to meet the gateway and
    /// open the session with its [`Hello`], whose reader sends each [`Arrival`] on `arrivals` as
    /// `wrap`, run on it, makes it; only [`Arrival::Exited`] names its session. It takes ops of at
    /// most `ops` bytes, if given ([`Arrival::Refused`]), and model requests if `models`.
    ///
    /// # Errors
    ///
    /// `InvalidData` for a first frame that is no hello it takes, else `TimedOut` past `by`;
    /// `Other` at once if `stopped` returns true as it waits; or if setting up fails.
    pub fn accept<T: Send + 'static>(
        self,
        by: Deadline,
        ops: Option<OpPayloadLimit>,
        models: bool,
        arrivals: Sender<T>,
        wrap: impl Fn(Arrival) -> T + Send + 'static,
        stopped: impl Fn() -> bool,
    ) -> io::Result<Session> {
        let Met { connection, gpus } = match self.meeting {
            Meeting::Socket(listener) => listener.connected(by, &stopped)?,
        };
        welcome(&connection, by)?;
        // Unbounded, so the reader never waits on its owner; the protocol bounds what each
        // session leaves there: the op in flight, the model requests in flight, at most
        // `MAX_MODEL_REQUESTS_IN_FLIGHT` of them, the exit of each command the owner started, and
        // the end. A receiver reused for the next session still holds what a dropped one left,
        // until its owner drains it. An owner that has dropped its receiver hears nothing more,
        // as it chose.
        let send = move |arrival| {
            let _ = arrivals.send(wrap(arrival));
        };
        Session::new(connection, gpus, ops, models, Box::new(send))
    }
}

/// Fails at once if `stopped` returns true, saying that the accept was stopped before `awaited`,
/// and that it expected `expected` before the stop.
fn unstopped(
    stopped: &dyn Fn() -> bool,
    awaited: impl fmt::Display,
    expected: &str,
) -> io::Result<()> {
    // Asked before each look, so that a stopped accept takes no connection, even one that waits
    // already. The stop is final, so its kind is `Other`: `Interrupted` would say that a retry
    // may succeed.
    if stopped() {
        return Err(io::Error::other(format!(
            "the accept was stopped before {awaited}: expected {expected} before the stop"
        )));
    }
    Ok(())
}

/// The socket one sandbox connects to, which identifies the sandbox, so it takes a [`Hello`] of
/// this version that presents no grant.
#[derive(Debug)]
struct Listener {
    listener: UnixListener,
    path: PathBuf,
}

impl Listener {
    /// Listens at [`SOCKET_NAME`] in `dir`, whose path may be of any length. Any user may
    /// connect, since a sandbox's user is not the gateway's; only the sandbox can reach `dir`.
    fn bind(dir: &Path) -> io::Result<Self> {
        let path = dir.join(SOCKET_NAME);
        // A socket's path, as `bind` takes it, is at most 107 bytes, which a deep directory's
        // passes; the kernel resolves `/proc/self/fd/<n>` to the directory that handle `n` holds,
        // so the path through it is short wherever the directory is.
        let bound = File::open(dir)
            .and_then(|handle| {
                let fd = handle.as_raw_fd();
                UnixListener::bind(format!("{OPEN_FILES}/{fd}/{SOCKET_NAME}"))
            })
            .and_then(|listener| {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o666))?;
                Ok(listener)
            });
        let listener = bound.map_err(|error| {
            let what = format_args!("the session's socket can't be made at {}", path.display());
            context(what, &error)
        })?;
        Ok(Self { listener, path })
    }

    /// Waits until `by`, or until `stopped` returns true, for the sandbox to connect, then removes
    /// the socket, and reads the sandbox's hello, until then too: one that presents no grant, as
    /// a socket of the sandbox's own identifies it.
    fn connected(self, by: Deadline, stopped: &dyn Fn() -> bool) -> io::Result<Met> {
        let path = self.path.display();
        self.listener
            .set_nonblocking(true)
            .map_err(|error| context(format_args!("{path} can't be made non-blocking"), &error))?;
        let stream = loop {
            let awaited = format_args!("a sandbox connected to {path}");
            unstopped(stopped, awaited, "its sandbox to connect")?;
            match self.listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if by.remaining().is_none() {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!(
                                "no sandbox connected to {path} by the deadline: expected its \
                                 sandbox to connect while the deadline lasts"
                            ),
                        ));
                    }
                    thread::sleep(POLL);
                }
                Err(error) => {
                    let what = format_args!("a connection to {path} can't be accepted");
                    return Err(context(what, &error));
                }
            }
        };
        // The socket goes at once, so that no second connection waits on it while the hello is
        // read.
        drop(self);
        let connection = Connection::Unix(stream);
        let hello = read_hello(&connection, by, stopped);
        let hello = hello.map_err(|error| refuse(&connection, by, error))?;
        let Hello { grant: None, gpus } = hello else {
            let message = "the hello presents a registration grant on a socket of the sandbox's \
                           own, which identifies it: expected none there";
            let error = io::Error::new(io::ErrorKind::InvalidData, message);
            return Err(refuse(&connection, by, error));
        };
        Ok(Met { connection, gpus })
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Removing the socket keeps a second connection out; a socket already gone is fine.
        let _ = fs::remove_file(&self.path);
    }
}

/// Reads the sandbox's first frame on `connection`, waiting for it until `by`, or until `stopped`
/// returns true: its [`Hello`], or the error that says why the session can't open, of kind
/// `InvalidData` for a first frame the gateway refuses ([`refuse`]), and `Other` for the stop. A
/// frame that waits already once `by` has passed is still read. Leaves `connection` blocking.
fn read_hello(
    connection: &Connection,
    by: Deadline,
    stopped: &dyn Fn() -> bool,
) -> io::Result<Hello> {
    // Read without blocking, as the connection was waited for, so that the session's reads,
    // which block, keep no bound of the hello's.
    connection.set_nonblocking(true).map_err(|error| {
        context(
            "the sandbox's connection can't be made non-blocking",
            &error,
        )
    })?;
    let mut polled = Polled {
        connection,
        by,
        stopped,
        stop: None,
    };
    let hello = frame::read_hello(&mut polled);
    connection
        .set_nonblocking(false)
        .map_err(|error| context("the sandbox's connection can't be made blocking", &error))?;
    // The stop's, which says what it stopped, as the accept's own does.
    if let Some(stop) = polled.stop {
        return Err(stop);
    }
    match hello {
        Ok(Some(hello)) => Ok(hello),
        Ok(None) => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the sandbox closed the connection before its hello: expected it to open its session \
             with one",
        )),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the sandbox closed the connection inside its first frame: expected a whole hello",
        )),
        Err(error) if error.kind() == io::ErrorKind::TimedOut => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the sandbox connected but sent no hello by the deadline: expected its sandboxd to \
             send one as it connects, as a sandboxd built from the same Pumpkin does",
        )),
        // Its message says why the gateway refuses the frame, as the sandbox is to be told.
        Err(error) if error.kind() == io::ErrorKind::InvalidData => Err(error),
        Err(error) => Err(context("the sandbox's hello can't be read", &error)),
    }
}

/// Returns the error of a session that doesn't open on `connection`, for `error`, which
/// [`read_hello`] gave, telling the sandbox why while `by` lasts if the gateway
/// refuses its first frame. The connection closes all the same.
fn refuse(connection: &Connection, by: Deadline, error: io::Error) -> io::Error {
    if error.kind() != io::ErrorKind::InvalidData {
        return error;
    }
    let message = error.to_string();
    let reason = format!("the gateway refused the session: {message}");
    let _ = frame::write(
        &mut Until { connection, by },
        &HelloReply::Refused { message },
        &[],
    );
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

/// Accepts the session on `connection`, whose hello opens one, telling the sandbox so while `by`
/// lasts.
fn welcome(connection: &Connection, by: Deadline) -> io::Result<()> {
    let accepted = HelloReply::Accepted;
    frame::write(&mut Until { connection, by }, &accepted, &[]).map_err(|error| {
        if !passed(&error) {
            return context("the answer to the sandbox's hello can't be sent", &error);
        }
        io::Error::new(
            io::ErrorKind::TimedOut,
            "the deadline passed before the sandbox's hello was answered: expected time left to \
             answer it",
        )
    })
}

/// Reads from a non-blocking connection, waiting for its bytes until `by`, or until `stopped`
/// returns true, as the accept waits for the connection, after which every read fails: as timed
/// out, or as stopped, keeping the stop's error ([`unstopped`]) in `stop`.
struct Polled<'a> {
    connection: &'a Connection,
    by: Deadline,
    stopped: &'a dyn Fn() -> bool,
    stop: Option<io::Error>,
}

impl Read for Polled<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match (&mut &*self.connection).read(buf) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if let Err(stop) = unstopped(self.stopped, "the sandbox's hello", "its hello") {
                        // Kept apart from the error returned, which a reader of the frame may
                        // wrap, so that the stop is told by where it came from, not by its kind.
                        let kind = stop.kind();
                        self.stop = Some(stop);
                        return Err(kind.into());
                    }
                    if self.by.remaining().is_none() {
                        return Err(io::ErrorKind::TimedOut.into());
                    }
                    thread::sleep(POLL);
                }
                read => return read,
            }
        }
    }
}
