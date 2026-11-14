//! Where a sandbox meets the gateway, and the hello that opens its session there.
//!
//! A sandbox meets the gateway where its [`Registration`] says: at a socket of its own, which
//! only it can reach, in its run's [`Sockets`], or at its run's TCP address, a
//! [`GrantListener`], where the grant its hello presents tells it apart; or the gateway dials it,
//! at the endpoint its provider gives, through a [`Dialer`]. Either way, the registration
//! accepts the [`Session`] the sandbox opens with its hello.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, DirBuilder, File};
use std::io::{self, Read};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use schema::ir::{GpuModel, Name};
use schema::sandbox::{
    Endpoint, HELLO_TIMEOUT, Hello, HelloReply, Listening, OpPayloadLimit,
    REGISTRATION_GRANT_BYTES, RegistrationGrant, SOCKET_NAME, SocketDial, TcpDial, frame,
};
use stdx::Deadline;

use crate::{Arrival, Connection, Session, Until, context, lock, passed};

/// The directory that names each file this process holds open by its descriptor's number,
/// through which a path resolves as through the file, a directory included.
const OPEN_FILES: &str = "/proc/self/fd";

/// The kernel's random source, which the driver's host, a full one, has.
const RANDOM: &str = "/dev/urandom";

/// How long a wait sleeps between its looks: a registration's accept for its sandbox, a hello's
/// read for its bytes, and a [`GrantListener`] for a connection, or its own drop.
const POLL: Duration = Duration::from_millis(10);

/// How long a [`Dialer`] waits before it dials a sandbox again, at first; each wait doubles the
/// last, up to [`REDIAL_MOST`].
// As gRPC's connection backoff starts (`doc/connection-backoff.md`, INITIAL_BACKOFF): a sandbox's
// `sandboxd` listens seconds after its create, and the provider's edge may refuse a dial until
// then.
const REDIAL_FIRST: Duration = Duration::from_secs(1);

/// The longest a [`Dialer`] waits before it dials a sandbox again.
// Shorter than gRPC's 120 s, as a start's window is minutes, of which a wait spends little.
const REDIAL_MOST: Duration = Duration::from_secs(15);

/// The most connections whose hellos a [`GrantListener`] reads at once.
// Each of a run's sandboxes dials once, and says hello as it connects, so a run needs far fewer.
const MAX_HELLOS: usize = 100;

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

/// The TCP address of a run's [`GrantListener`], which it binds and its sandboxes dial.
///
/// It is any address but an unspecified one, such as `0.0.0.0`, or `[::ffff:0.0.0.0]`, its
/// IPv4-mapped form, which a listener binds but no sandbox can dial. A deployment names the port
/// that its network admits sandboxes on, such as its security group's, or port 0, for one that the
/// kernel picks from its ephemeral range as the listener binds, which the listener's dials then
/// name: so tests on the loopback, and a deployment whose network admits that range.
#[derive(Debug)]
pub struct GatewayAddress(SocketAddr);

impl TryFrom<SocketAddr> for GatewayAddress {
    type Error = String;

    fn try_from(address: SocketAddr) -> Result<Self, Self::Error> {
        if address.ip().to_canonical().is_unspecified() {
            return Err(format!(
                "the gateway's address {address} is unspecified, which no sandbox can dial: name \
                 the address its sandboxes dial"
            ));
        }
        Ok(Self(address))
    }
}

/// One TCP address that every sandbox of a run dials, where the grant each one's hello presents
/// tells them apart.
///
/// [`issue`](Self::issue) issues a [`RegistrationGrant`] for a sandbox, whose [`Registration`]
/// accepts its session. The listener refuses a hello of this version that presents no grant, one
/// it doesn't await, or one presented already, saying which. Dropping it stops it taking
/// connections.
// Until a peer's hello presents a grant it awaits, the peer costs little: the listener reads at
// most a hello's bytes from it, within its hello timeout of taking its connection, then closes the
// connection untold; and it reads at most `MAX_HELLOS` hellos at once, closing a connection beyond
// them as it takes it, unread and untold.
#[derive(Debug)]
pub struct GrantListener {
    /// The address that the listener is bound to, which its sandboxes dial.
    gateway: SocketAddr,
    // Shared with the thread that takes connections, the threads that read their hellos, and the
    // registrations, which wait for the connections that present their grants.
    grants: Arc<Mutex<Grants>>,
    /// The thread that takes connections, `Some` until the drop joins it.
    listening: Option<JoinHandle<()>>,
}

/// What a [`GrantListener`] awaits and reads.
#[derive(Debug, Default)]
struct Grants {
    /// The grants whose registrations last, by the number each took at its issue.
    awaited: BTreeMap<u64, Awaited>,
    /// The number the next grant takes.
    next: u64,
    /// How many connections' hellos are being read.
    hellos: usize,
    /// Whether the listener has dropped, which ends its taking connections.
    closed: bool,
}

/// A grant whose registration lasts, and the connection that presented it, once one has.
#[derive(Debug)]
struct Awaited {
    grant: RegistrationGrant,
    presented: Option<Met>,
}

/// A sandbox's connection, once its hello has been read, and the GPUs that the hello names.
#[derive(Debug)]
struct Met {
    connection: Connection,
    gpus: Option<Vec<GpuModel>>,
}

impl GrantListener {
    /// Listens at `address`, taking connections on a thread of its own until it drops, and gives
    /// each `hello_timeout` from when it takes it to say its hello ([`HELLO_TIMEOUT`] is the
    /// protocol's).
    ///
    /// # Errors
    ///
    /// Fails if `address` can't be bound, or the listener can't be made not to block, or if the
    /// thread doesn't start.
    pub fn bind(address: &GatewayAddress, hello_timeout: Duration) -> io::Result<Self> {
        let GatewayAddress(address) = *address;
        let listener = TcpListener::bind(address).map_err(|error| {
            context(
                format_args!("the grant listener can't bind {address}"),
                &error,
            )
        })?;
        // As bound, with the port the kernel picked for port 0.
        let gateway = listener
            .local_addr()
            .map_err(|error| context("the grant listener's address can't be read", &error))?;
        // Polled, so that the thread notices the listener's drop.
        listener
            .set_nonblocking(true)
            .map_err(|error| context("the grant listener can't be made non-blocking", &error))?;
        let grants = Arc::new(Mutex::new(Grants::default()));
        let shared = Arc::clone(&grants);
        let listening = thread::Builder::new()
            .name("grants".to_owned())
            .spawn(move || listen(&listener, &shared, hello_timeout))
            .map_err(|error| context("the grant listener's thread didn't start", &error))?;
        Ok(Self {
            gateway,
            grants,
            listening: Some(listening),
        })
    }

    /// Issues a sandbox a grant, of random bytes from the kernel, as the [`TcpDial`] its
    /// sandbox dials, and the registration that accepts the session the grant opens, as long as
    /// the registration lasts.
    ///
    /// # Errors
    ///
    /// Fails if the kernel's random source can't be read.
    pub fn issue(&self) -> io::Result<(TcpDial, Registration)> {
        let grant = random_grant()?;
        let mut grants = lock(&self.grants);
        let number = grants.next;
        grants.next += 1;
        let awaited = Awaited {
            grant: grant.clone(),
            presented: None,
        };
        grants.awaited.insert(number, awaited);
        let granted = Granted {
            grants: Arc::clone(&self.grants),
            number,
        };
        let meeting = Meeting::Grant(granted);
        let dial = TcpDial {
            gateway: self.gateway,
            grant,
        };
        Ok((dial, Registration { meeting }))
    }
}

impl Drop for GrantListener {
    fn drop(&mut self) {
        lock(&self.grants).closed = true;
        if let Some(listening) = self.listening.take()
            && listening.join().is_err()
        {
            unreachable!(
                "the grant listener's thread panicked, though a panic aborts the process \
                 (`stdx::abort_on_panic`), so the thread never ends in one"
            )
        }
    }
}

/// Takes connections on `listener` until its [`GrantListener`] drops, reading each one's hello,
/// within `hello_timeout`, on a thread of its own, so that a silent peer holds up no other, unless
/// [`MAX_HELLOS`] are being read already: it then closes the connection, unread and untold, as the
/// peer may dial again.
fn listen(listener: &TcpListener, grants: &Arc<Mutex<Grants>>, hello_timeout: Duration) {
    while !lock(grants).closed {
        match listener.accept() {
            Ok((stream, _)) => admit(grants, stream, Deadline::after(hello_timeout)),
            // None waits, or one was reset before it was taken, or the process has no file
            // descriptor to spare: each passes, and the kernel keeps the rest waiting.
            Err(_) => thread::sleep(POLL),
        }
    }
}

/// Reads the hello of `stream` by `by` on a thread of its own, unless [`MAX_HELLOS`] are being
/// read already or the thread doesn't start: the connection then closes, unread.
fn admit(grants: &Arc<Mutex<Grants>>, stream: TcpStream, by: Deadline) {
    let mut locked = lock(grants);
    if locked.hellos >= MAX_HELLOS {
        return;
    }
    locked.hellos += 1;
    drop(locked);
    let reader = Arc::clone(grants);
    let spawned = thread::Builder::new()
        .name("hello".to_owned())
        .spawn(move || register(&reader, stream, by));
    // A thread that doesn't start drops its closure, which closes the connection.
    if spawned.is_err() {
        lock(grants).hellos -= 1;
    }
}

/// Reads the hello of `stream` by `by`, and hands its connection to the registration of the grant
/// it presents, if that grant opens a session: any other peer is turned away. Frees the hello's
/// place as it returns.
fn register(grants: &Mutex<Grants>, stream: TcpStream, by: Deadline) {
    let set = stdx::net::set_options(&stream)
        .map_err(|error| context("the connection's TCP options can't be set", &error));
    let connection = Connection::Tcp(stream);
    // Only its own deadline ends the read: no accept waits on it until it presents a grant.
    let unopened = match set.and_then(|()| read_hello(&connection, by, &|| false)) {
        Ok(Hello { grant, gpus }) => present(grants, grant, Met { connection, gpus }).err(),
        Err(error) => Some((connection, error)),
    };
    // No accept waits on a connection that opens no session, so its error goes no further.
    if let Some((connection, error)) = unopened {
        refuse(&connection, by, error);
    }
    // Freed last, so that the threads reading hellos, or refusing them, stay within bounds.
    lock(grants).hellos -= 1;
}

/// Hands `met`, whose hello presents `grant`, to the registration that awaits that grant, or
/// gives back its connection with its refusal ([`refuse`]). A grant is found and taken under one
/// lock, so no two connections take one.
fn present(
    grants: &Mutex<Grants>,
    grant: Option<RegistrationGrant>,
    met: Met,
) -> Result<(), (Connection, io::Error)> {
    let refused = |message| io::Error::new(io::ErrorKind::InvalidData, message);
    let Some(grant) = grant else {
        let message = "the hello presents no registration grant: expected the one the gateway \
                       issued for the sandbox, at an address that several sandboxes share";
        return Err((met.connection, refused(message)));
    };
    let mut locked = lock(grants);
    // By the grants' `==`, which takes as long whichever bytes differ, as they hold no order
    // that a map could look one up by.
    let awaited = (locked.awaited.values_mut()).find(|awaited| awaited.grant == grant);
    let message = match awaited {
        Some(Awaited {
            grant: _,
            presented: presented @ None,
        }) => {
            *presented = Some(met);
            return Ok(());
        }
        Some(Awaited {
            grant: _,
            presented: Some(_),
        }) => {
            "the registration grant was presented already, on another connection: expected it \
             once, as a grant opens one session"
        }
        None => {
            "the registration grant opens no session: expected one the gateway issued for a \
             sandbox whose start waits, presented once"
        }
    };
    Err((met.connection, refused(message)))
}

/// Where the gateway dials the sandboxes that listen ([`Listening`]), each at the endpoint its
/// create gives, over TLS with its client, which checks the endpoint's certificate.
///
/// A registration's accept dials until its deadline, as often as a dial fails, so a slow start, or
/// a slow link, makes the accept wait, and only the deadline fails it, saying why the last dial
/// failed. TLS that the client refuses, as a certificate it doesn't trust for the endpoint's name,
/// and a hello that the gateway refuses, fail it at once.
// Each dial waits within `HELLO_TIMEOUT` for its connection, its TLS and its hello, then the next
// waits a second, then twice the last wait, at most `REDIAL_MOST`. A stop is heeded between a
// dial's steps, its lookup of the name, which the resolver's own timeouts bound, each connection,
// and its TLS, and as it waits for the hello.
#[derive(Debug)]
pub struct Dialer {
    client: tls::Client,
}

impl Dialer {
    pub fn new(client: tls::Client) -> Self {
        Self { client }
    }

    /// Issues a sandbox a grant, of random bytes from the kernel, as the [`Listening`] its
    /// `sandboxd` listens for, and what makes, of the endpoint its create gives, the registration
    /// that accepts its session.
    ///
    /// # Errors
    ///
    /// Fails if the kernel's random source can't be read.
    pub fn issue(&self) -> io::Result<(Listening, Unreached)> {
        let grant = random_grant()?;
        let unreached = Unreached {
            client: self.client.clone(),
            grant: grant.clone(),
        };
        Ok((Listening { grant }, unreached))
    }
}

/// A grant that a [`Dialer`] issued, for a sandbox whose create is yet to give its endpoint.
#[derive(Debug)]
pub struct Unreached {
    client: tls::Client,
    grant: RegistrationGrant,
}

impl Unreached {
    /// The registration that dials the sandbox at `endpoint`, and presents the grant there.
    pub fn at(self, endpoint: Endpoint) -> Registration {
        let Self { client, grant } = self;
        Registration {
            meeting: Meeting::Dial(Dialed {
                client,
                grant,
                endpoint,
            }),
        }
    }
}

/// A [`Dialer`]'s registration of one sandbox: where it dials it, and the grant it presents.
#[derive(Debug)]
struct Dialed {
    client: tls::Client,
    grant: RegistrationGrant,
    endpoint: Endpoint,
}

/// What a stopped [`Dialed`] expected of its sandbox.
const LISTENING: &str = "its sandboxd to listen there";

impl Dialed {
    /// What a stopped dial awaited.
    fn awaited(&self) -> impl fmt::Display {
        format!("the gateway reached the sandbox at {}", self.endpoint)
    }

    /// Dials the sandbox until `by`, or until `stopped` returns true, as [`Dialer`] says, and
    /// reads its hello: one that presents no grant, as the endpoint identifies the sandbox.
    fn connected(&self, by: Deadline, stopped: &dyn Fn() -> bool) -> io::Result<Met> {
        let endpoint = &self.endpoint;
        let late = |failed: &dyn fmt::Display| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "the gateway didn't reach the sandbox at {endpoint} by the deadline: \
                     {failed}: expected its sandboxd to listen there while the deadline lasts"
                ),
            )
        };
        let mut wait = REDIAL_FIRST;
        let mut failed = None;
        loop {
            unstopped(stopped, self.awaited(), LISTENING)?;
            let Some(left) = by.remaining() else {
                return Err(match failed {
                    Some(error) => late(&format_args!("its last dial failed: {error}")),
                    None => late(&"it passed before the first dial"),
                });
            };
            match self.dial(Deadline::after(left.min(HELLO_TIMEOUT)), stopped) {
                Ok(met) => return Ok(met),
                Err(DialError::Again(error)) => failed = Some(error),
                Err(DialError::Never(error)) => return Err(error),
            }
            // Waited out in looks, so that a stop is heeded within one, and none outlasts `by`.
            let until = Deadline::after(wait);
            while until.remaining().is_some() && by.remaining().is_some() {
                unstopped(stopped, self.awaited(), LISTENING)?;
                thread::sleep(POLL);
            }
            wait = (wait * 2).min(REDIAL_MOST);
        }
    }

    /// Dials the sandbox once, by `by`, and reads its hello, until then too.
    fn dial(&self, by: Deadline, stopped: &dyn Fn() -> bool) -> Result<Met, DialError> {
        let endpoint = &self.endpoint;
        let stream = self.connect(by, stopped)?;
        unstopped(stopped, self.awaited(), LISTENING).map_err(DialError::Never)?;
        let secured = self.client.connect(stream, endpoint.host(), by);
        let connection = match secured {
            Ok(stream) => Connection::Tls(stream),
            // TLS that isn't what the client trusts, which another dial would meet again.
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                let what =
                    format_args!("the gateway can't trust the sandbox's endpoint {endpoint}");
                return Err(DialError::Never(context(what, &error)));
            }
            Err(error) => return Err(DialError::Again(error)),
        };
        match read_hello(&connection, by, stopped) {
            Ok(Hello { grant: None, gpus }) => Ok(Met { connection, gpus }),
            Ok(Hello { grant: Some(_), .. }) => {
                let message = "the hello presents a registration grant where the gateway dialed \
                               the sandbox, which its endpoint identifies: expected none there";
                let error = io::Error::new(io::ErrorKind::InvalidData, message);
                Err(DialError::Never(refuse(&connection, by, error)))
            }
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                Err(DialError::Never(refuse(&connection, by, error)))
            }
            // A stop among them, which the next look heeds.
            Err(error) => Err(DialError::Again(error)),
        }
    }

    /// The TCP connection to the endpoint, made by `by`, with its options set.
    fn connect(&self, by: Deadline, stopped: &dyn Fn() -> bool) -> Result<TcpStream, DialError> {
        let endpoint = &self.endpoint;
        let dialing = format_args!("dialing {endpoint} failed");
        // The resolver's own timeouts and attempts (`resolv.conf(5)`) bound the lookup.
        let addresses = (endpoint.host(), endpoint.port())
            .to_socket_addrs()
            .map_err(|error| DialError::Again(context(dialing, &error)))?;
        let mut failed = io::Error::new(
            io::ErrorKind::NotFound,
            format!("{endpoint} names no address: expected its name to resolve"),
        );
        for address in addresses {
            unstopped(stopped, self.awaited(), LISTENING).map_err(DialError::Never)?;
            let Some(left) = by.remaining() else {
                break;
            };
            match TcpStream::connect_timeout(&address, left) {
                Ok(stream) => {
                    stdx::net::set_options(&stream).map_err(|error| {
                        DialError::Never(context(
                            "the connection's TCP options can't be set",
                            &error,
                        ))
                    })?;
                    return Ok(stream);
                }
                Err(error) => failed = context(dialing, &error),
            }
        }
        Err(DialError::Again(failed))
    }
}

/// Why a [`Dialed`]'s dial failed.
enum DialError {
    /// Why this dial failed, which another may not.
    Again(io::Error),
    /// Why the session can't open, which no other dial changes: the stop among them.
    Never(io::Error),
}

/// A grant of random bytes from the kernel.
fn random_grant() -> io::Result<RegistrationGrant> {
    let mut bytes = [0; REGISTRATION_GRANT_BYTES];
    File::open(RANDOM)
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|error| {
            let what = format_args!(
                "the kernel's random source {RANDOM} can't be read: expected it to give a \
                 registration grant's bytes"
            );
            context(what, &error)
        })?;
    Ok(RegistrationGrant::from(bytes))
}

/// Where one sandbox meets the gateway, and the accept of the session it opens there.
///
/// [`Sockets::issue`] and [`GrantListener::issue`] issue it, and [`Unreached::at`] makes it. The
/// sandbox meets the gateway as it connects to its socket, as its hello presents its grant, or as
/// the gateway dials it. Dropping the registration ends the meeting: it removes the socket, or
/// ends the grant, closing any connection that presented it.
#[derive(Debug)]
pub struct Registration {
    meeting: Meeting,
}

/// Where a [`Registration`]'s sandbox meets the gateway.
#[derive(Debug)]
enum Meeting {
    Socket(Listener),
    Grant(Granted),
    Dial(Dialed),
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
        let (Met { connection, gpus }, grant) = match self.meeting {
            Meeting::Socket(listener) => (listener.connected(by, &stopped)?, None),
            Meeting::Grant(granted) => (granted.presented(by, &stopped)?, None),
            Meeting::Dial(dialed) => (dialed.connected(by, &stopped)?, Some(dialed.grant)),
        };
        welcome(&connection, by, grant)?;
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

/// A [`GrantListener`]'s registration of one grant: the grant it awaits under its number.
#[derive(Debug)]
struct Granted {
    grants: Arc<Mutex<Grants>>,
    number: u64,
}

impl Granted {
    /// Waits until `by`, or until `stopped` returns true, for a connection that presents the
    /// grant, and takes it, which ends the grant.
    fn presented(&self, by: Deadline, stopped: &dyn Fn() -> bool) -> io::Result<Met> {
        loop {
            let awaited = "a sandbox presented the registration grant";
            unstopped(
                stopped,
                awaited,
                "the sandbox it was issued for to present it",
            )?;
            let mut grants = lock(&self.grants);
            let Some(awaited) = grants.awaited.get_mut(&self.number) else {
                unreachable!(
                    "a registration's grant is not awaited while it waits, though a grant is \
                     awaited from its issue until its registration takes the connection that \
                     presented it, here, which then returns, or drops; and the registration's \
                     accept takes it by value"
                )
            };
            if let Some(met) = awaited.presented.take() {
                grants.awaited.remove(&self.number);
                return Ok(met);
            }
            drop(grants);
            if by.remaining().is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "no sandbox presented the registration grant by the deadline: expected the \
                     sandbox it was issued for to present it while the deadline lasts",
                ));
            }
            // Nothing tells the wait of a presentation, or of a stop, so it looks again.
            thread::sleep(POLL);
        }
    }
}

impl Drop for Granted {
    fn drop(&mut self) {
        // The grant ends here, unless a connection presented it and was taken, closing the
        // connection that presented it, if any.
        lock(&self.grants).awaited.remove(&self.number);
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
/// [`read_hello`] or [`present`] gave, telling the sandbox why while `by` lasts if the gateway
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
fn welcome(
    connection: &Connection,
    by: Deadline,
    grant: Option<RegistrationGrant>,
) -> io::Result<()> {
    let accepted = HelloReply::Accepted { grant };
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
