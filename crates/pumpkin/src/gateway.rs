//! The gateway's end of the sessions that sandboxes open; the protocol is
//! [`pumpkin_schema::sandbox`].
//!
//! Each sandbox gets a [`Listener`] of its own: a Unix socket that only that sandbox can
//! reach. The first connection on it becomes the sandbox's [`Session`] and the socket is
//! removed, so no second connection can follow. The socket, not a secret, tells which sandbox
//! is talking.
//!
//! Everything a session receives is untrusted, since the sandbox may run an agent's code. A
//! message that fails to parse or comes out of turn, more replies than a request takes, or
//! silence for [`REPLY_TIMEOUT`] while a request waits ends the session, which stops the
//! sandbox. A collected tree is written into a new directory of its own, within [`Limits`].

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use pumpkin_schema::ir::AbsolutePath;
use pumpkin_schema::sandbox::tree::{self, Limits};
use pumpkin_schema::sandbox::{
    Entry, ExitStatus, FromSandbox, Launch, RelativePath, Signal, ToSandbox, frame,
};

/// How long a request waits for the sandbox's next frame. `sandboxd` answers every request
/// promptly, and sends a tree's frames one after another, so a sandbox silent this long is
/// stuck or hostile.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// How many frames of a tree may wait for the request writing it. While they wait, the session
/// reads nothing more.
const QUEUED_TREE_FRAMES: usize = 64;

/// How many frames may wait for any other request: its reply and, for a command, its exit.
const QUEUED_REPLIES: usize = 2;

/// The socket one sandbox connects to.
#[derive(Debug)]
pub struct Listener {
    listener: UnixListener,
    path: PathBuf,
}

impl Listener {
    /// Listens at `path`. Any user may connect, since a sandbox's user is not the gateway's;
    /// only the sandbox can reach the socket's directory.
    ///
    /// # Errors
    ///
    /// Fails if the socket can't be created.
    pub fn bind(path: PathBuf) -> io::Result<Self> {
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666))?;
        Ok(Self { listener, path })
    }

    /// Waits up to `timeout` for the sandbox to connect, then removes the socket.
    ///
    /// # Errors
    ///
    /// Fails if nothing connects in time or accepting fails.
    pub fn accept(self, timeout: Duration) -> io::Result<Session> {
        self.listener.set_nonblocking(true)?;
        let deadline = Instant::now() + timeout;
        let stream = loop {
            match self.listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!(
                                "no sandbox connected to {} within {timeout:?}",
                                self.path.display()
                            ),
                        ));
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        };
        stream.set_nonblocking(false)?;
        Session::new(stream)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Removing the socket keeps a second connection out; a socket already gone is fine.
        let _ = fs::remove_file(&self.path);
    }
}

/// A sandbox's session.
///
/// Requests on it block until `sandboxd` answers or the session ends. Dropping it ends the
/// session, which stops the sandbox.
#[derive(Debug)]
pub struct Session {
    writer: Mutex<UnixStream>,
    /// The connection, for ending the session without waiting for a write in progress.
    connection: UnixStream,
    routes: Arc<Mutex<Routes>>,
    next_stream: AtomicU32,
}

/// Where frames from the sandbox go: the requests waiting for them, by stream.
#[derive(Debug, Default)]
struct Routes {
    open: HashMap<u32, Route>,
    /// Why the session ended, once it has.
    ended: Option<String>,
}

/// The way to a request waiting for frames.
#[derive(Clone, Debug)]
struct Route {
    sender: SyncSender<Reply>,
    queue: Queue,
}

/// What a request waits for.
#[derive(Clone, Copy, Debug)]
enum Queue {
    /// A reply and, for a command, its exit: a frame more breaks the protocol.
    Replies,
    /// A tree, whose frames wait for the request to write them.
    Tree,
}

type Reply = (FromSandbox, Vec<u8>);

/// What to put in a sandbox: a directory, or a file with its bytes.
#[derive(Clone, Copy, Debug)]
pub enum Put<'a> {
    Dir(&'a RelativePath),
    File(&'a RelativePath, &'a [u8]),
}

/// A command started in a sandbox.
#[derive(Debug)]
pub struct Process {
    stream: u32,
    replies: Receiver<Reply>,
    routes: Arc<Mutex<Routes>>,
    exited: Option<ExitStatus>,
}

impl Drop for Process {
    fn drop(&mut self) {
        lock(&self.routes).open.remove(&self.stream);
    }
}

impl Session {
    fn new(stream: UnixStream) -> io::Result<Self> {
        let routes = Arc::new(Mutex::new(Routes::default()));
        let reader = stream.try_clone()?;
        let reader_routes = Arc::clone(&routes);
        thread::Builder::new()
            .name("session".to_owned())
            .spawn(move || route(reader, &reader_routes))?;
        Ok(Self {
            connection: stream.try_clone()?,
            writer: Mutex::new(stream),
            routes,
            next_stream: AtomicU32::new(1),
        })
    }

    /// Writes `entries` below `dir` in the sandbox, creating `dir` and its missing parents.
    /// Each entry's parent is `dir` or comes earlier in `entries`, unless it exists already.
    ///
    /// # Errors
    ///
    /// Fails if `sandboxd` can't write them or the session ends.
    pub fn put(&self, dir: &AbsolutePath, entries: &[Put<'_>]) -> io::Result<()> {
        let (stream, replies) = self.open(Queue::Replies)?;
        let put = self
            .send_tree(stream, dir, entries)
            .and_then(|()| self.done(&replies));
        self.close(stream);
        put
    }

    /// Starts a command.
    ///
    /// # Errors
    ///
    /// Fails if the command doesn't start or the session ends.
    pub fn start(&self, launch: Launch) -> io::Result<Process> {
        let (stream, replies) = self.open(Queue::Replies)?;
        let started = self
            .send(&ToSandbox::Start { stream, launch }, &[])
            .and_then(|()| self.done(&replies));
        match started {
            Ok(()) => Ok(Process {
                stream,
                replies,
                routes: Arc::clone(&self.routes),
                exited: None,
            }),
            Err(error) => {
                self.close(stream);
                Err(error)
            }
        }
    }

    /// Waits up to `timeout` for a command to exit: `None` if it is still running.
    ///
    /// # Errors
    ///
    /// Fails if the session ends first.
    pub fn wait(&self, process: &mut Process, timeout: Duration) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = process.exited {
            return Ok(Some(status));
        }
        match process.replies.recv_timeout(timeout) {
            Ok((FromSandbox::Exited { status, .. }, _)) => {
                self.close(process.stream);
                process.exited = Some(status);
                Ok(Some(status))
            }
            Ok((other, _)) => Err(unexpected(&other)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(self.ended()),
        }
    }

    /// Signals a running command's process group.
    ///
    /// # Errors
    ///
    /// Fails if the command isn't running or the session ends.
    pub fn signal(&self, process: &Process, signal: Signal) -> io::Result<()> {
        let command = process.stream;
        self.request(|stream| ToSandbox::Signal {
            stream,
            command,
            signal,
        })
    }

    /// Kills every process in the sandbox but `sandboxd`, so that nothing collected afterwards
    /// changes, and returns once they are gone.
    ///
    /// # Errors
    ///
    /// Fails if `sandboxd` can't kill them or the session ends.
    pub fn freeze(&self) -> io::Result<()> {
        self.request(|stream| ToSandbox::Freeze { stream })
    }

    /// Copies the tree in the sandbox's `dir` into `into`, a new directory, within `limits`.
    /// After a failure, `into` holds whatever arrived before it.
    ///
    /// # Errors
    ///
    /// Fails if `into` exists, if `sandboxd` can't send the tree, if the tree breaks the
    /// protocol or passes a limit, or if the session ends.
    pub fn collect(&self, dir: &AbsolutePath, into: &Path, limits: Limits) -> io::Result<()> {
        fs::create_dir(into)?;
        let (stream, replies) = self.open(Queue::Tree)?;
        let tree = tree::Writer::new(into.to_owned(), limits);
        let collected = self
            .send(
                &ToSandbox::Collect {
                    stream,
                    dir: dir.clone(),
                },
                &[],
            )
            .and_then(|()| self.receive_tree(&replies, tree));
        self.close(stream);
        collected
    }

    fn send_tree(&self, stream: u32, dir: &AbsolutePath, entries: &[Put<'_>]) -> io::Result<()> {
        self.send(
            &ToSandbox::Put {
                stream,
                dir: dir.clone(),
            },
            &[],
        )?;
        for entry in entries {
            match *entry {
                Put::Dir(path) => {
                    let entry = Entry::Dir { path: path.clone() };
                    self.send(&ToSandbox::Entry { stream, entry }, &[])?;
                }
                Put::File(path, bytes) => {
                    let size = u64::try_from(bytes.len()).map_err(io::Error::other)?;
                    let entry = Entry::File {
                        path: path.clone(),
                        size,
                    };
                    self.send(&ToSandbox::Entry { stream, entry }, &[])?;
                    for chunk in bytes.chunks(frame::MAX_BODY_BYTES) {
                        self.send(&ToSandbox::Data { stream }, chunk)?;
                    }
                }
            }
        }
        self.send(&ToSandbox::End { stream }, &[])
    }

    fn receive_tree(&self, replies: &Receiver<Reply>, mut tree: tree::Writer) -> io::Result<()> {
        loop {
            match self.receive(replies)? {
                (FromSandbox::Entry { entry, .. }, _) => tree.entry(&entry)?,
                (FromSandbox::Data { .. }, body) => tree.data(&body)?,
                (FromSandbox::End { .. }, _) => return tree.finish(),
                (FromSandbox::Failed { message, .. }, _) => return Err(io::Error::other(message)),
                (other @ (FromSandbox::Done { .. } | FromSandbox::Exited { .. }), _) => {
                    return Err(unexpected(&other));
                }
            }
        }
    }

    /// Sends the request `message` builds for a new stream, and waits for its reply.
    fn request(&self, message: impl FnOnce(u32) -> ToSandbox) -> io::Result<()> {
        let (stream, replies) = self.open(Queue::Replies)?;
        let done = self
            .send(&message(stream), &[])
            .and_then(|()| self.done(&replies));
        self.close(stream);
        done
    }

    fn open(&self, queue: Queue) -> io::Result<(u32, Receiver<Reply>)> {
        let stream = self.next_stream.fetch_add(1, Ordering::Relaxed);
        let (sender, replies) = mpsc::sync_channel(match queue {
            Queue::Replies => QUEUED_REPLIES,
            Queue::Tree => QUEUED_TREE_FRAMES,
        });
        let mut routes = lock(&self.routes);
        if let Some(reason) = &routes.ended {
            return Err(ended(reason));
        }
        routes.open.insert(stream, Route { sender, queue });
        Ok((stream, replies))
    }

    fn close(&self, stream: u32) {
        lock(&self.routes).open.remove(&stream);
    }

    /// Sends a frame. If the session has ended, its reason is the error, rather than the
    /// failed write's.
    fn send(&self, message: &ToSandbox, body: &[u8]) -> io::Result<()> {
        frame::write(&mut *lock(&self.writer), message, body).map_err(|error| {
            let reason = lock(&self.routes).ended.clone();
            reason.map_or(error, |reason| ended(&reason))
        })
    }

    fn receive(&self, replies: &Receiver<Reply>) -> io::Result<Reply> {
        match replies.recv_timeout(REPLY_TIMEOUT) {
            Ok(reply) => Ok(reply),
            Err(RecvTimeoutError::Timeout) => {
                let silent = format!("the sandbox sent nothing for {REPLY_TIMEOUT:?}");
                end(&self.routes, &self.connection, silent);
                Err(self.ended())
            }
            Err(RecvTimeoutError::Disconnected) => Err(self.ended()),
        }
    }

    fn done(&self, replies: &Receiver<Reply>) -> io::Result<()> {
        match self.receive(replies)? {
            (FromSandbox::Done { .. }, _) => Ok(()),
            (FromSandbox::Failed { message, .. }, _) => Err(io::Error::other(message)),
            (
                other @ (FromSandbox::Exited { .. }
                | FromSandbox::Entry { .. }
                | FromSandbox::Data { .. }
                | FromSandbox::End { .. }),
                _,
            ) => Err(unexpected(&other)),
        }
    }

    fn ended(&self) -> io::Error {
        let routes = lock(&self.routes);
        ended(routes.ended.as_deref().unwrap_or("the session is ending"))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Closing the connection ends the session; the reader thread then sees it close.
        let _ = self.connection.shutdown(Shutdown::Both);
    }
}

/// Reads frames from the sandbox until the session ends, handing each to the request
/// waiting on its stream. Frames for no waiting request are dropped: their request has
/// given up.
fn route(connection: UnixStream, routes: &Mutex<Routes>) {
    let mut reader = BufReader::new(connection);
    let reason = loop {
        let (message, body) = match frame::read::<FromSandbox>(&mut reader) {
            Ok(Some(frame)) => frame,
            Ok(None) => break "the sandbox ended the session".to_owned(),
            Err(error) => break format!("the session failed: {error}"),
        };
        let stream = stream_of(&message);
        let Some(route) = lock(routes).open.get(&stream).cloned() else {
            continue;
        };
        match route.queue {
            // The request writes the tree as it reads it, or drops the queue when it fails.
            Queue::Tree => drop(route.sender.send((message, body))),
            Queue::Replies => match route.sender.try_send((message, body)) {
                Ok(()) | Err(TrySendError::Disconnected(_)) => {}
                Err(TrySendError::Full(_)) => {
                    break format!(
                        "the sandbox broke the protocol: it sent more on stream {stream} than \
                         a request takes"
                    );
                }
            },
        }
    };
    end(routes, reader.get_ref(), reason);
}

/// Ends the session, unless it has ended already: records why, wakes every request waiting on
/// it, and closes the connection, which stops the sandbox.
fn end(routes: &Mutex<Routes>, connection: &UnixStream, reason: String) {
    let mut routes = lock(routes);
    routes.ended.get_or_insert(reason);
    routes.open.clear();
    drop(routes);
    // A connection already closed is fine.
    let _ = connection.shutdown(Shutdown::Both);
}

fn stream_of(message: &FromSandbox) -> u32 {
    match *message {
        FromSandbox::Done { stream }
        | FromSandbox::Failed { stream, .. }
        | FromSandbox::Exited { stream, .. }
        | FromSandbox::Entry { stream, .. }
        | FromSandbox::Data { stream }
        | FromSandbox::End { stream } => stream,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn ended(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, reason.to_owned())
}

fn unexpected(message: &FromSandbox) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("the sandbox broke the protocol: it sent {message:?} out of turn"),
    )
}
