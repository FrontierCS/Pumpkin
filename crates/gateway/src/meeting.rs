//! How sandboxes meet the gateway.
//!
//! A meeting issues one [`Registration`] for a sandbox about to start, and the registration's
//! `accept` waits for that sandbox, reads its hello and opens its [`Session`]. Dropping a
//! registration unspent withdraws what its meeting issued. What identifies a sandbox is how it
//! meets, as [the sandbox protocol](schema::sandbox) says.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, DirBuilder, File};
use std::io::{self, Read};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use schema::frame;
use schema::ir::{GpuModel, Name};
use schema::sandbox::{
    Attestation, Endpoint, HELLO_TIMEOUT, Hello, HelloReply, Listening, OpPayloadLimit,
    PrivateDial, REGISTRATION_GRANT_BYTES, RegistrationGrant, SOCKET_NAME, SocketDial,
};
use stdx::Deadline;

use crate::{Arrival, Connection, Session, Until, context, lock, passed};

// Names each file this process holds open by its descriptor's number: a path through one resolves
// as through the file, a directory included. A socket's path holds 107 bytes at most, which a deep
// directory's may pass, while the path below a directory's descriptor is short wherever it is.
const OPEN_FILES: &str = "/proc/self/fd";

const RANDOM: &str = "/dev/urandom";

// How long a wait sleeps between looks at its stop, its deadline and what it waits for.
const POLL: Duration = Duration::from_millis(10);

// A dial that fails is tried again after this wait, which doubles up to `REDIAL_MOST`. It starts
// as gRPC's connection backoff does, at its INITIAL_BACKOFF of 1 s:
// https://github.com/grpc/grpc/blob/785ff9afd6410c535a44690e7cca1f81228d8851/doc/connection-backoff.md#L36-L38
// A sandbox's `sandboxd` listens seconds after its create, and the provider's edge may refuse a
// dial until then.
const REDIAL_FIRST: Duration = Duration::from_secs(1);

// Shorter than gRPC's 120 s, as a start's window is minutes, of which a wait spends little.
const REDIAL_MOST: Duration = Duration::from_secs(15);

// Bounds the threads that connections hold before they have said who they are: the listener's
// address is open to whatever can reach it, and each connection holds a thread until its hello is
// read and admitted or refused, or its time is up. Each of a run's sandboxes dials once, and says
// hello as it connects, so a run needs far fewer. A hello that no check admits keeps its thread
// while a registration issued before it awaits its check, until that registration's deadline, so
// hellos that no sandbox says may hold every thread for as long as a create.
const MAX_HELLOS: usize = 100;

// Bounds the reasons that a refusal of a hello that no check admits gives, of as many as the
// registrations that await one: checks that refuse alike give one reason, so a few show why.
const MAX_REASONS: usize = 4;

/// Gives each sandbox a socket of its own below one directory, which dropping removes.
#[derive(Debug)]
pub struct Sockets {
    dir: PathBuf,
}

impl Sockets {
    /// Makes `dir`, which must not exist yet, in a directory where no other user may rename
    /// entries: one who could would swap `dir` for a directory of their own, where the sockets
    /// would then be issued. Its path may be of any length.
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

    /// Makes the directory and the socket of the sandbox `name`, which is new, and returns where
    /// the sandbox dials. The socket takes the first connection, whoever makes it, which identifies
    /// the sandbox, so show the directory to that sandbox alone.
    pub fn issue(&self, name: &Name) -> io::Result<(SocketDial, Registration)> {
        // A name is one path component, neither `.` nor `..`, so each sandbox's directory is
        // directly within this one: none is outside it, and none holds another's.
        let dir = self.dir.join(name.as_str());
        fs::create_dir(&dir).map_err(|error| context(dir.display(), &error))?;
        // Set, not left to the umask, which may take the traverse away. The sandbox's user isn't
        // this one, and may share its group: traversing is all it needs, to reach the socket it
        // knows by name. The run's directory, 0700, keeps every other local user out.
        let traverse = fs::Permissions::from_mode(0o711);
        fs::set_permissions(&dir, traverse).map_err(|error| {
            let what = format_args!("the mode of {} can't be set", dir.display());
            context(what, &error)
        })?;
        let listener = SocketListener::bind(&dir)?;
        let registration = Registration {
            meeting: Meeting::Socket(listener),
        };
        Ok((SocketDial { dir }, registration))
    }
}

impl Drop for Sockets {
    fn drop(&mut self) {
        // Each socket has gone with its registration, so this removes empty directories, which hold
        // nothing a run needs: a failure to remove them is left unreported.
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The TCP address of a run's [`Listener`], which it binds and its sandboxes dial.
///
/// It is any address but an unspecified one, such as `0.0.0.0`, or `[::ffff:0.0.0.0]`, its
/// IPv4-mapped form, which a listener binds but no sandbox can dial. The port is the one that the
/// network admits sandboxes on, or 0, for one that the kernel picks as the listener binds, which
/// the listener's dials then name: 0 suits the loopback, or a network that admits the kernel's
/// ephemeral range.
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

/// Listens at one address on a private network that every sandbox of a run dials.
///
/// It tells the sandboxes apart by the [`Attestation`] that each hello presents and the address
/// it comes from, which the check of each registration that it issued admits or not
/// ([`Unattested::admitting`]). The first registration whose check admits a hello takes it. It
/// refuses a hello that presents no attestation, one that a registration admits after it has
/// admitted another, and one that no check admits, saying which; a hello that no check admits
/// while a registration issued before it still awaits its check waits for that check, until the
/// registration's deadline. Dropping it stops the listening; sessions already accepted live on.
// A peer costs little until a check admits its hello: at most a hello's bytes and a thread, within
// its hello timeout and the deadlines of the registrations that await their checks, before the
// connection closes untold.
#[derive(Debug)]
pub struct Listener {
    gateway: SocketAddr,
    registry: Arc<Mutex<Registry>>,
    listening: Option<JoinHandle<()>>,
}

#[derive(Debug, Default)]
struct Registry {
    awaited: BTreeMap<u64, Awaited>,
    next: u64,
    hellos: usize,
    closed: bool,
}

#[derive(Debug)]
struct Awaited {
    // None until the registration is given its check.
    check: Option<Check>,
    // Until when a hello that no check admits waits for this registration's check.
    by: Deadline,
    presented: Option<Met>,
    // Why its check refused the last hello that no check admitted, for its accept to tell.
    refused: Option<String>,
}

type Admits = dyn Fn(&Attestation, IpAddr) -> Result<(), String> + Send;

struct Check(Box<Admits>);

impl fmt::Debug for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Check(..)")
    }
}

#[derive(Debug)]
struct Met {
    connection: Connection,
    gpus: Option<Vec<GpuModel>>,
}

impl Listener {
    /// Binds `address`, and gives each connection `hello_timeout` to say its hello
    /// ([`HELLO_TIMEOUT`] is the protocol's), from when it takes the connection. Where the port of
    /// `address` is 0, the dials issued name the port that was bound.
    pub fn bind(address: &GatewayAddress, hello_timeout: Duration) -> io::Result<Self> {
        let GatewayAddress(address) = *address;
        let listener = TcpListener::bind(address).map_err(|error| {
            context(
                format_args!("the private listener can't bind {address}"),
                &error,
            )
        })?;
        let gateway = listener
            .local_addr()
            .map_err(|error| context("the private listener's address can't be read", &error))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| context("the private listener can't be made non-blocking", &error))?;
        let registry = Arc::new(Mutex::new(Registry::default()));
        let shared = Arc::clone(&registry);
        let listening = thread::Builder::new()
            .name("private".to_owned())
            .spawn(move || listen(&listener, &shared, hello_timeout))
            .map_err(|error| context("the private listener's thread didn't start", &error))?;
        Ok(Self {
            gateway,
            registry,
            listening: Some(listening),
        })
    }

    /// Issues the dial of one sandbox, and its registration, which awaits the check that admits
    /// the sandbox's hello until `by`, and takes no hello until it has it.
    pub fn issue(&self, by: Deadline) -> (PrivateDial, Unattested) {
        let mut registry = lock(&self.registry);
        let number = registry.next;
        registry.next += 1;
        let awaited = Awaited {
            check: None,
            by,
            presented: None,
            refused: None,
        };
        registry.awaited.insert(number, awaited);
        let awaiting = Awaiting {
            registry: Arc::clone(&self.registry),
            number,
        };
        let dial = PrivateDial {
            gateway: self.gateway,
        };
        (dial, Unattested { awaiting })
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        lock(&self.registry).closed = true;
        if let Some(listening) = self.listening.take()
            && listening.join().is_err()
        {
            unreachable!(
                "the private listener's thread panicked, though a panic aborts the process \
                 (`stdx::abort_on_panic`), so the thread never ends in one"
            )
        }
    }
}

/// A registration of a [`Listener`]'s that awaits the check that admits its sandbox's hello,
/// which the provider makes only as it creates the sandbox.
#[derive(Debug)]
pub struct Unattested {
    awaiting: Awaiting,
}

impl Unattested {
    /// Registers `check`, which says whether a hello's attestation, from the address given, is
    /// this sandbox's, or why not. It is asked once of each hello that no registration has taken,
    /// under the listener's lock, so it must return at once, as a check of a signature does.
    ///
    /// Its reason is told to the hello's sender, and to this registration's accept, if no check
    /// admits the hello; so that the checks that refuse one hello give few reasons, its reason
    /// names nothing of its own sandbox's that the hello doesn't.
    pub fn admitting(
        self,
        check: impl Fn(&Attestation, IpAddr) -> Result<(), String> + Send + 'static,
    ) -> Registration {
        let Self { awaiting } = self;
        let mut registry = lock(&awaiting.registry);
        let Some(awaited) = registry.awaited.get_mut(&awaiting.number) else {
            unreachable!(
                "an unattested registration is not awaited, though it is from its issue until \
                 it drops, and only its registration's accept, which this makes, takes it"
            )
        };
        awaited.check = Some(Check(Box::new(check)));
        drop(registry);
        Registration {
            meeting: Meeting::Private(awaiting),
        }
    }
}

fn listen(listener: &TcpListener, registry: &Arc<Mutex<Registry>>, hello_timeout: Duration) {
    // The listener doesn't block, so that `closed` is seen.
    while !lock(registry).closed {
        match listener.accept() {
            Ok((stream, peer)) => {
                // As an IPv4 address, where a socket of both families gives it mapped to IPv6.
                let peer = peer.ip().to_canonical();
                admit(registry, stream, peer, hello_timeout);
            }
            // None waits, or one was reset before it was taken, or the process has no file
            // descriptor to spare: each passes, and the kernel keeps the rest waiting.
            Err(_) => thread::sleep(POLL),
        }
    }
}

fn admit(registry: &Arc<Mutex<Registry>>, stream: TcpStream, peer: IpAddr, timeout: Duration) {
    let mut locked = lock(registry);
    // Past the bound the connection closes unread and untold: the peer may dial again.
    if locked.hellos >= MAX_HELLOS {
        return;
    }
    locked.hellos += 1;
    drop(locked);
    let reader = Arc::clone(registry);
    let spawned = thread::Builder::new()
        .name("hello".to_owned())
        .spawn(move || register(&reader, stream, peer, timeout));
    // A thread that doesn't start drops its closure, which closes the connection.
    if spawned.is_err() {
        lock(registry).hellos -= 1;
    }
}

fn register(registry: &Mutex<Registry>, stream: TcpStream, peer: IpAddr, timeout: Duration) {
    let by = Deadline::after(timeout);
    let set = stdx::net::set_options(&stream)
        .map_err(|error| context("the connection's TCP options can't be set", &error));
    let connection = Connection::Tcp(stream);
    // Only its own deadline ends the read: no accept waits on it until a check admits it.
    let passed = || by.remaining().is_none();
    let unopened = match set.and_then(|()| read_hello(&connection, &passed, &|| false)) {
        Ok((Hello { gpus }, attestation)) => {
            present(registry, attestation, peer, Met { connection, gpus }).err()
        }
        Err(error) => Some((connection, error)),
    };
    // No accept waits on a connection that opens no session, so its error goes no further. The
    // refusal gets a time of its own, as the hello may have waited past its own for a check.
    if let Some((connection, error)) = unopened {
        refuse(&connection, Deadline::after(timeout), error);
    }
    // Freed last, so that the threads reading hellos, or refusing them, stay within bounds.
    lock(registry).hellos -= 1;
}

/// Why a hello that no check admits is refused, from the distinct `reasons` of the checks.
fn said_no(reasons: &BTreeSet<String>) -> String {
    let expected = "expected the attestation of a sandbox whose start waits, presented once";
    let mut said: Vec<&str> = reasons
        .iter()
        .take(MAX_REASONS)
        .map(String::as_str)
        .collect();
    let more = format!("{} more", reasons.len().saturating_sub(MAX_REASONS));
    if reasons.len() > MAX_REASONS {
        said.push(&more);
    }
    if said.is_empty() {
        return format!(
            "the attestation opens no session, as no sandbox's start waits: {expected}"
        );
    }
    format!(
        "the attestation opens no session: {}: {expected}",
        said.join("; ")
    )
}

fn present(
    registry: &Mutex<Registry>,
    attestation: Option<Attestation>,
    peer: IpAddr,
    met: Met,
) -> Result<(), (Connection, io::Error)> {
    let refused = |message: String| io::Error::new(io::ErrorKind::InvalidData, message);
    let Some(attestation) = attestation else {
        let message = "the hello presents no attestation: expected the sandbox's, at an address \
                       that several sandboxes share";
        return Err((met.connection, refused(message.to_owned())));
    };
    // Each registration's check is asked once, as what it answers never changes: why each refused.
    let mut reasons = BTreeMap::new();
    // The hello waits only for the checks of registrations issued before it was read, as a
    // sandbox's registration is issued before its create, so that later ones keep no hello.
    let issued = lock(registry).next;
    loop {
        // A check admits and its registration is given the connection under one lock, so no two
        // connections are given one.
        let mut locked = lock(registry);
        let mut unchecked = false;
        let mut admitting = None;
        for (&number, awaited) in &locked.awaited {
            let Some(Check(check)) = &awaited.check else {
                unchecked |= number < issued && awaited.by.remaining().is_some();
                continue;
            };
            if reasons.contains_key(&number) {
                continue;
            }
            match check(&attestation, peer) {
                Ok(()) => {
                    admitting = Some(number);
                    break;
                }
                Err(reason) => {
                    reasons.insert(number, reason);
                }
            }
        }
        // The listener's drop ends the wait for a check.
        unchecked &= !locked.closed;
        let message = match admitting.and_then(|number| locked.awaited.get_mut(&number)) {
            Some(Awaited {
                presented: presented @ None,
                ..
            }) => {
                *presented = Some(met);
                return Ok(());
            }
            Some(Awaited {
                presented: Some(_), ..
            }) => {
                "the attestation was presented already, on another connection: expected it once, \
                 as it opens one session"
                    .to_owned()
            }
            // Nothing wakes the wait on a check, so it looks again.
            None if unchecked => {
                drop(locked);
                thread::sleep(POLL);
                continue;
            }
            None => {
                let mut distinct = BTreeSet::new();
                for (number, reason) in reasons {
                    distinct.insert(reason.clone());
                    if let Some(awaited) = locked.awaited.get_mut(&number) {
                        awaited.refused = Some(reason);
                    }
                }
                said_no(&distinct)
            }
        };
        return Err((met.connection, refused(message)));
    }
}

/// Where the gateway dials the sandboxes that listen ([`Listening`]), each at the endpoint its
/// create gives, over TLS with its client, which checks the endpoint's certificate.
///
/// A registration's accept dials until its deadline, as often as a dial fails, so a slow start, or
/// a slow link, makes the accept wait, and only the deadline fails it, saying why the last dial
/// failed. TLS that the client refuses, as a certificate it doesn't trust for the endpoint's name,
/// and a hello that the gateway refuses, fail it at once.
#[derive(Debug)]
pub struct Dialer {
    client: tls::Client,
}

impl Dialer {
    pub fn new(client: tls::Client) -> Self {
        Self { client }
    }

    /// Issues the grant of one sandbox, in the [`Listening`] that its `sandboxd` is given, and an
    /// [`Unreached`] that takes the sandbox's endpoint, which the provider learns only by creating
    /// the sandbox, and so after the grant is given.
    pub fn issue(&self) -> io::Result<(Listening, Unreached)> {
        let grant = random_grant()?;
        let unreached = Unreached {
            client: self.client.clone(),
            grant: grant.clone(),
        };
        Ok((Listening { grant }, unreached))
    }
}

/// A grant issued for a sandbox whose endpoint isn't known yet.
#[derive(Debug)]
pub struct Unreached {
    client: tls::Client,
    grant: RegistrationGrant,
}

impl Unreached {
    /// Registers the endpoint where the sandbox listens.
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

#[derive(Debug)]
struct Dialed {
    client: tls::Client,
    grant: RegistrationGrant,
    endpoint: Endpoint,
}

const LISTENING: &str = "its sandboxd to listen there";

impl Dialed {
    fn awaited(&self) -> impl fmt::Display {
        format!("the gateway reached the sandbox at {}", self.endpoint)
    }

    fn connected(&self, by: Bound<'_>, stopped: &dyn Fn() -> bool) -> io::Result<Met> {
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
            let Some(left) = by.left() else {
                return Err(match failed {
                    Some(error) => late(&format_args!("its last dial failed: {error}")),
                    None => late(&"it passed before the first dial"),
                });
            };
            // A dial gets the hello timeout at most, so that one that hangs leaves time for
            // another.
            match self.dial(Deadline::after(left.min(HELLO_TIMEOUT)), stopped) {
                Ok(met) => return Ok(met),
                Err(DialError::Again(error)) => failed = Some(error),
                Err(DialError::Never(error)) => return Err(error),
            }
            // Waited out in looks, so that a stop is heeded within one, and none outlasts `by`.
            let until = Deadline::after(wait);
            while until.remaining().is_some() && by.left().is_some() {
                unstopped(stopped, self.awaited(), LISTENING)?;
                thread::sleep(POLL);
            }
            wait = (wait * 2).min(REDIAL_MOST);
        }
    }

    fn dial(&self, by: Deadline, stopped: &dyn Fn() -> bool) -> Result<Met, DialError> {
        let endpoint = &self.endpoint;
        let stream = self.connect(by, stopped)?;
        unstopped(stopped, self.awaited(), LISTENING).map_err(DialError::Never)?;
        let secured = self.client.connect(stream, endpoint.host(), by);
        let connection = match secured {
            Ok(stream) => Connection::Tls(stream),
            // TLS's own failures are `InvalidData`, a certificate that isn't the endpoint's among
            // them: another dial would meet the same.
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                let what =
                    format_args!("the gateway can't trust the sandbox's endpoint {endpoint}");
                return Err(DialError::Never(context(what, &error)));
            }
            Err(error) => return Err(DialError::Again(error)),
        };
        match read_hello(&connection, &|| by.remaining().is_none(), stopped) {
            Ok((Hello { gpus }, None)) => Ok(Met { connection, gpus }),
            Ok((_, Some(_))) => {
                let message = "the hello presents an attestation where the gateway dialed the \
                               sandbox, which its endpoint identifies: expected none there";
                let error = io::Error::new(io::ErrorKind::InvalidData, message);
                Err(DialError::Never(refuse(&connection, by, error)))
            }
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                Err(DialError::Never(refuse(&connection, by, error)))
            }
            Err(error) => Err(DialError::Again(error)),
        }
    }

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

// `Again` where a later dial may succeed, as the sandbox may not listen yet; `Never` where it
// can't.
enum DialError {
    Again(io::Error),
    Never(io::Error),
}

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
/// Dropping the registration ends the meeting: it removes the socket, or ends the wait for the
/// sandbox's hello, closing any connection that its check admitted.
#[derive(Debug)]
pub struct Registration {
    meeting: Meeting,
}

#[derive(Debug)]
enum Meeting {
    Socket(SocketListener),
    Private(Awaiting),
    Dial(Dialed),
}

impl Registration {
    /// Waits for the sandbox to meet the gateway, accepts its hello, and returns its session.
    ///
    /// `ops` is the session's limit on an op's payload, none for a session that takes no ops, and
    /// `models` whether it takes model requests. The session's reader thread sends each arrival, as
    /// `wrap` makes it, to `arrivals`, and drops it once `arrivals`' receiver has gone.
    ///
    /// `left` gives the time left by the caller's clock, as [`Deadline::remaining`] does by the
    /// machine's. Once it gives none the call fails with `TimedOut`, and once `stopped` says yes it
    /// fails too. A hello that is wrong, which the sandbox is told of, fails it with `InvalidData`.
    pub fn accept<T: Send + 'static>(
        self,
        left: impl Fn() -> Option<Duration>,
        ops: Option<OpPayloadLimit>,
        models: bool,
        arrivals: Sender<T>,
        wrap: impl Fn(Arrival) -> T + Send + 'static,
        stopped: impl Fn() -> bool,
    ) -> io::Result<Session> {
        let by = Bound { left: &left };
        let (Met { connection, gpus }, grant) = match self.meeting {
            Meeting::Socket(listener) => (listener.connected(by, &stopped)?, None),
            Meeting::Private(awaiting) => (awaiting.presented(by, &stopped)?, None),
            // A `sandboxd` that listens serves only a peer that answers its hello with its grant.
            Meeting::Dial(dialed) => (dialed.connected(by, &stopped)?, Some(dialed.grant)),
        };
        welcome(&connection, by.deadline(), grant)?;
        // `arrivals` is unbounded, so the reader never waits on its owner; the protocol bounds what
        // a session leaves there: the op in flight, the model requests in flight, at most
        // `MAX_MODEL_REQUESTS_IN_FLIGHT`, the exit of each command the owner started, and the end.
        // An owner that has dropped its receiver hears nothing more, as it chose.
        let send = move |arrival| {
            let _ = arrivals.send(wrap(arrival));
        };
        Session::new(connection, gpus, ops, models, Box::new(send))
    }
}

// The time left of an accept, by its caller's clock.
#[derive(Clone, Copy)]
struct Bound<'a> {
    left: &'a dyn Fn() -> Option<Duration>,
}

impl Bound<'_> {
    // The time left, none once it has passed.
    fn left(self) -> Option<Duration> {
        (self.left)()
    }

    // The deadline by the machine's clock at the time left now, for a socket's waits: a dial's,
    // and an answer's to a hello.
    fn deadline(self) -> Deadline {
        Deadline::after(self.left().unwrap_or_default())
    }
}

fn unstopped(
    stopped: &dyn Fn() -> bool,
    awaited: impl fmt::Display,
    expected: &str,
) -> io::Result<()> {
    // Asked before each look, so that a stopped accept takes no connection, even one that waits
    // already. The stop is final, so its kind is `Other`: `Interrupted` would say that a retry may
    // succeed.
    if stopped() {
        return Err(io::Error::other(format!(
            "the accept was stopped before {awaited}: expected {expected} before the stop"
        )));
    }
    Ok(())
}

#[derive(Debug)]
struct SocketListener {
    listener: UnixListener,
    path: PathBuf,
}

impl SocketListener {
    fn bind(dir: &Path) -> io::Result<Self> {
        let path = dir.join(SOCKET_NAME);
        let bound = File::open(dir)
            .and_then(|handle| {
                let fd = handle.as_raw_fd();
                UnixListener::bind(format!("{OPEN_FILES}/{fd}/{SOCKET_NAME}"))
            })
            .and_then(|listener| {
                // Any user may connect, since a sandbox's user is not the gateway's; only the
                // sandbox can reach `dir`.
                fs::set_permissions(&path, fs::Permissions::from_mode(0o666))?;
                Ok(listener)
            });
        let listener = bound.map_err(|error| {
            let what = format_args!("the session's socket can't be made at {}", path.display());
            context(what, &error)
        })?;
        Ok(Self { listener, path })
    }

    fn connected(self, by: Bound<'_>, stopped: &dyn Fn() -> bool) -> io::Result<Met> {
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
                    if by.left().is_none() {
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
        let hello = read_hello(&connection, &|| by.left().is_none(), stopped);
        let hello = hello.map_err(|error| refuse(&connection, by.deadline(), error))?;
        let (Hello { gpus }, None) = hello else {
            let message = "the hello presents an attestation on a socket of the sandbox's own, \
                           which identifies it: expected none there";
            let error = io::Error::new(io::ErrorKind::InvalidData, message);
            return Err(refuse(&connection, by.deadline(), error));
        };
        Ok(Met { connection, gpus })
    }
}

impl Drop for SocketListener {
    fn drop(&mut self) {
        // Removing the socket keeps a second connection out; a socket already gone is fine.
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Debug)]
struct Awaiting {
    registry: Arc<Mutex<Registry>>,
    number: u64,
}

impl Awaiting {
    fn presented(&self, by: Bound<'_>, stopped: &dyn Fn() -> bool) -> io::Result<Met> {
        loop {
            let awaited = "the sandbox's hello was admitted";
            unstopped(stopped, awaited, "the sandbox to say it")?;
            let mut registry = lock(&self.registry);
            let Some(awaited) = registry.awaited.get_mut(&self.number) else {
                unreachable!(
                    "a registration is not awaited while it waits, though it is awaited from its \
                     issue until it takes the connection that its check admitted, here, which \
                     then returns, or drops; and the registration's accept takes it by value"
                )
            };
            if let Some(met) = awaited.presented.take() {
                registry.awaited.remove(&self.number);
                return Ok(met);
            }
            if by.left().is_none() {
                let last = match &awaited.refused {
                    Some(reason) => {
                        format!(", and it refused the last hello that none admitted: {reason}")
                    }
                    None => String::new(),
                };
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "no hello that the sandbox's check admits came by the deadline{last}: \
                         expected the sandbox to say it while the deadline lasts"
                    ),
                ));
            }
            drop(registry);
            // Nothing wakes the wait on a presentation or a stop, so it looks again.
            thread::sleep(POLL);
        }
    }
}

impl Drop for Awaiting {
    fn drop(&mut self) {
        // The registration ends here, unless it took a connection, closing the connection that its
        // check admitted, if any.
        lock(&self.registry).awaited.remove(&self.number);
    }
}

// `passed` says whether the read's deadline has passed.
fn read_hello(
    connection: &Connection,
    passed: &dyn Fn() -> bool,
    stopped: &dyn Fn() -> bool,
) -> io::Result<(Hello, Option<Attestation>)> {
    // Read without blocking, as the connection was waited for, so that a stop and the deadline
    // interrupt the wait; blocking again after, as the session's reads carry none of its bounds.
    connection.set_nonblocking(true).map_err(|error| {
        context(
            "the sandbox's connection can't be made non-blocking",
            &error,
        )
    })?;
    let mut polled = Polled {
        connection,
        passed,
        stopped,
        stop: None,
    };
    let hello = schema::sandbox::read_hello(&mut polled);
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

fn refuse(connection: &Connection, by: Deadline, error: io::Error) -> io::Error {
    // Only a hello that is wrong is refused to the sandbox: after any other failure the connection
    // can't be trusted to carry an answer.
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

struct Polled<'a> {
    connection: &'a Connection,
    passed: &'a dyn Fn() -> bool,
    stopped: &'a dyn Fn() -> bool,
    // Kept apart from the error returned, which a reader of the frame may wrap, so that the stop is
    // told by where it came from, not by its kind.
    stop: Option<io::Error>,
}

impl Read for Polled<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match (&mut &*self.connection).read(buf) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if let Err(stop) = unstopped(self.stopped, "the sandbox's hello", "its hello") {
                        let kind = stop.kind();
                        self.stop = Some(stop);
                        return Err(kind.into());
                    }
                    if (self.passed)() {
                        return Err(io::ErrorKind::TimedOut.into());
                    }
                    thread::sleep(POLL);
                }
                read => return read,
            }
        }
    }
}
