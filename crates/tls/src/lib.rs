//! TLS 1.3 below a session whose connection crosses the Internet: a [`Client`]'s end, which
//! trusts a server only by its certificate, checked against the roots it trusts and the name it
//! dialed, and a [`Server`]'s, which shows its certificate chain. A client may also run TLS within
//! another connection's [`Stream`], to a server that the stream's far end carries it to
//! ([`Client::connect_within`]).
//!
//! Each end hands its session a [`Stream`]: a Unix socket of its own, whose other end a relay
//! thread carries to and from the TLS connection, so the session reads, writes, times out and
//! shuts down as on any socket, from as many threads as it likes, and the relay never waits on
//! one way while the other has bytes to move. The session's end of its socket closes the TLS
//! connection, with TLS's `close_notify`, and the connection below it; the peer's
//! `close_notify` ends the session's reads, and any other end of the connection below, or a break
//! in its TLS, fails them, saying why ([`Stream`]).

use std::io::{self, IoSlice, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

use rustix::event::{self, PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, TrustAnchor};
use rustls::{
    ClientConfig, ClientConnection, ConfigBuilder, Connection, RootCertStore, ServerConfig,
    ServerConnection, WantsVerifier,
};
use stdx::Deadline;

/// The most bytes the relay reads from the session's socket at once: a TLS record's most
/// plaintext.
const CHUNK_BYTES: usize = 16 * 1024;

/// How long the relay of a session that has ended still sends what it holds, and its
/// `close_notify`, once the session's end of its socket has closed.
// The session has written its last; what remains is at most a chunk's records, which a link
// that moves anything at all sends within this. A peer that reads nothing more holds it no longer.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(15);

/// A TLS client, and the roots it trusts, which its clones share.
#[derive(Clone, Debug)]
pub struct Client {
    config: Arc<ClientConfig>,
}

impl Client {
    /// A client that trusts `roots` alone, such as Web PKI's, which `webpki-roots` ships.
    pub fn trusting_roots(roots: &[TrustAnchor<'static>]) -> Self {
        let roots = RootCertStore {
            roots: roots.to_vec(),
        };
        Self::with(roots)
    }

    /// A client that trusts `root`, a certificate in DER, alone: one that can't be a root fails.
    pub fn trusting(root: &[u8]) -> io::Result<Self> {
        let mut roots = RootCertStore::empty();
        (roots.add(CertificateDer::from(root.to_vec()))).map_err(|error| tls(&error))?;
        Ok(Self::with(roots))
    }

    fn with(roots: RootCertStore) -> Self {
        let mut config = builder(ClientConfig::builder_with_provider(provider()))
            .with_root_certificates(roots)
            .with_no_client_auth();
        // Each session is a connection of its own, which nothing resumes.
        config.resumption = rustls::client::Resumption::disabled();
        Self {
            config: Arc::new(config),
        }
    }

    /// Runs TLS on `stream`, a connection to the server `name`, a DNS name, which the server's
    /// certificate must name, with the handshake done by `by`, and returns the session's
    /// [`Stream`].
    ///
    /// # Errors
    ///
    /// Fails if `name` isn't a DNS name, if the server's certificate isn't one of `name` from a
    /// trusted root, or if the handshake fails otherwise or isn't done by `by`: TLS's own
    /// failures are [`InvalidData`](io::ErrorKind::InvalidData) alone.
    pub fn connect(&self, stream: TcpStream, name: &str, by: Deadline) -> io::Result<Stream> {
        self.carried(Wire::Tcp(stream), name, by)
    }

    /// Runs TLS within `outer`, another connection's stream, to the server `name` that the
    /// stream's far end carries it to. It checks that server as [`connect`](Self::connect) checks
    /// the one it dials, and fails alike.
    ///
    /// The returned stream ends `outer` as it ends. Why `outer` ended, it doesn't say: its reads
    /// then fail as on a connection cut short.
    pub fn connect_within(&self, outer: Stream, name: &str, by: Deadline) -> io::Result<Stream> {
        let Stream { socket, ended: _ } = outer;
        // Whatever its holder set, as the handshake's reads and writes wait until `by`.
        socket.set_nonblocking(false)?;
        self.carried(Wire::Within(socket), name, by)
    }

    fn carried(&self, wire: Wire, name: &str, by: Deadline) -> io::Result<Stream> {
        let Ok(server @ ServerName::DnsName(_)) = ServerName::try_from(name.to_owned()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "`{}` is not a DNS name: give a server's",
                    name.escape_debug()
                ),
            ));
        };
        let connection =
            ClientConnection::new(Arc::clone(&self.config), server).map_err(|error| tls(&error))?;
        let expected = format!("expected TLS 1.3 from `{name}`, with a certificate trusted for it");
        carry(Connection::Client(connection), wire, by, &expected)
    }
}

/// A TLS server, and the certificate chain it shows, which its clones share.
#[derive(Clone, Debug)]
pub struct Server {
    config: Arc<ServerConfig>,
}

impl Server {
    /// A server that shows `chain`, certificates in DER from its own to the last below a root,
    /// whose own is of `key`, a PKCS #8 private key in DER. A `key` that isn't a private key, or
    /// isn't the key of `chain`'s first certificate, fails.
    pub fn new(chain: &[&[u8]], key: &[u8]) -> io::Result<Self> {
        let chain = (chain.iter())
            .map(|certificate| CertificateDer::from(certificate.to_vec()))
            .collect();
        let key = PrivateKeyDer::try_from(key.to_vec()).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("the server's key can't be read: {error}: expected a PKCS #8 key in DER"),
            )
        })?;
        let mut config = builder(ServerConfig::builder_with_provider(provider()))
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .map_err(|error| tls(&error))?;
        // Each session is a connection of its own, which nothing resumes.
        config.send_tls13_tickets = 0;
        Ok(Self {
            config: Arc::new(config),
        })
    }

    /// Runs TLS on `stream`, a connection to a client, with the handshake done by `by`, and
    /// returns the session's [`Stream`]. A handshake not done by `by` fails.
    pub fn accept(&self, stream: TcpStream, by: Deadline) -> io::Result<Stream> {
        let connection =
            ServerConnection::new(Arc::clone(&self.config)).map_err(|error| tls(&error))?;
        carry(
            Connection::Server(connection),
            Wire::Tcp(stream),
            by,
            "expected a client's TLS 1.3",
        )
    }
}

fn provider() -> Arc<CryptoProvider> {
    // `ring`, the one backend that the workspace builds `rustls` with.
    Arc::new(rustls::crypto::ring::default_provider())
}

fn builder<S: rustls::ConfigSide>(
    builder: ConfigBuilder<S, rustls::WantsVersions>,
) -> ConfigBuilder<S, WantsVerifier> {
    let Ok(builder) = builder.with_protocol_versions(&[&rustls::version::TLS13]) else {
        unreachable!("ring's provider lacks TLS 1.3, though rustls's ring provider has it")
    };
    builder
}

fn tls(error: &rustls::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("TLS failed: {error}"))
}

/// The session's end of a TLS connection, a Unix socket, read and written as one.
///
/// A read that would give the socket's end fails instead, with why, once the TLS connection has
/// ended other than by the peer's `close_notify`. Its clones share the socket and that reason.
#[derive(Debug)]
pub struct Stream {
    socket: UnixStream,
    /// Why the TLS connection ended, set before the relay ends the socket, unset if by the
    /// peer's `close_notify`. Shared with the relay's thread, which sets it.
    ended: Arc<OnceLock<(io::ErrorKind, String)>>,
}

impl Stream {
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            socket: self.socket.try_clone()?,
            ended: Arc::clone(&self.ended),
        })
    }

    /// Ends the session's stream both ways, for every clone, which closes the TLS connection, as
    /// the [crate doc](crate) says.
    pub fn shutdown(&self) -> io::Result<()> {
        self.socket.shutdown(Shutdown::Both)
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.socket.set_nonblocking(nonblocking)
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.socket.set_write_timeout(timeout)
    }
}

// Its socket, for the options of a socket, such as a bound on its reads.
impl AsFd for Stream {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.socket.as_fd()
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self).read(buf)
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&*self).flush()
    }
}

impl Read for &Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = (&self.socket).read(buf)?;
        if read == 0
            && !buf.is_empty()
            && let Some((kind, why)) = self.ended.get()
        {
            return Err(io::Error::new(*kind, why.clone()));
        }
        Ok(read)
    }
}

impl Write for &Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&self.socket).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// What a TLS connection runs on: a TCP connection, or the session's end of the [`Stream`] of
/// another TLS connection.
#[derive(Debug)]
enum Wire {
    Tcp(TcpStream),
    Within(UnixStream),
}

impl Wire {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.set_read_timeout(timeout),
            Self::Within(socket) => socket.set_read_timeout(timeout),
        }
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.set_write_timeout(timeout),
            Self::Within(socket) => socket.set_write_timeout(timeout),
        }
    }

    fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.set_nonblocking(nonblocking),
            Self::Within(socket) => socket.set_nonblocking(nonblocking),
        }
    }

    /// Ends it both ways. That closes the other connection, as [`Stream::shutdown`] does.
    fn shutdown(&self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.shutdown(Shutdown::Both),
            Self::Within(socket) => socket.shutdown(Shutdown::Both),
        }
    }

    /// What has ended once a read of it gives its end.
    fn name(&self) -> &'static str {
        match self {
            Self::Tcp(_) => "the peer's TCP connection",
            Self::Within(_) => "the connection that carries this one",
        }
    }
}

impl AsFd for Wire {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match self {
            Self::Tcp(stream) => stream.as_fd(),
            Self::Within(socket) => socket.as_fd(),
        }
    }
}

// Through a shared reference, as `TcpStream` and `UnixStream` read and write.
impl Read for &Wire {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Wire::Tcp(stream) => (&mut &*stream).read(buf),
            Wire::Within(socket) => (&mut &*socket).read(buf),
        }
    }
}

impl Write for &Wire {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Wire::Tcp(stream) => (&mut &*stream).write(buf),
            Wire::Within(socket) => (&mut &*socket).write(buf),
        }
    }

    // The default writes the first of `bufs` alone, so a record a call, where `TcpStream` sends
    // a flight's records in one. Sent apart, a peer that refuses the flight may close with part
    // of it unread, which resets the connection under the alert that says why.
    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        match self {
            Wire::Tcp(stream) => (&mut &*stream).write_vectored(bufs),
            Wire::Within(socket) => (&mut &*socket).write_vectored(bufs),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Wire::Tcp(stream) => (&mut &*stream).flush(),
            Wire::Within(socket) => (&mut &*socket).flush(),
        }
    }
}

/// Completes the handshake by `by`, or fails saying what was `expected`, then starts the relay.
fn carry(mut tls: Connection, wire: Wire, by: Deadline, expected: &str) -> io::Result<Stream> {
    let late = || {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("the TLS handshake wasn't done by its deadline: {expected}"),
        )
    };
    let failed = |error: io::Error| match error.kind() {
        // How a socket's timeout ends a read or a write.
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => late(),
        kind => io::Error::new(
            kind,
            format!("the TLS handshake failed: {error}: {expected}"),
        ),
    };
    while tls.is_handshaking() || tls.wants_write() {
        let left = by.remaining().ok_or_else(late)?;
        wire.set_read_timeout(Some(left))?;
        wire.set_write_timeout(Some(left))?;
        tls.complete_io(&mut &wire).map_err(failed)?;
    }
    wire.set_read_timeout(None)?;
    wire.set_write_timeout(None)?;
    wire.set_nonblocking(true)?;
    let (socket, carried) = UnixStream::pair()?;
    carried.set_nonblocking(true)?;
    let ended = Arc::new(OnceLock::new());
    let relayed = Arc::clone(&ended);
    thread::Builder::new()
        .name("tls".to_owned())
        .spawn(move || relay(tls, &wire, &carried, &relayed))?;
    Ok(Stream { socket, ended })
}

/// Relays between `wire`, under `tls`, and `carried` until either ends, then closes both, having
/// set `ended` unless the peer ended the TLS with its `close_notify`.
fn relay(
    mut tls: Connection,
    wire: &Wire,
    carried: &UnixStream,
    ended: &OnceLock<(io::ErrorKind, String)>,
) {
    let mut closing = None;
    if let Err(error) = moved(&mut tls, wire, carried, &mut closing) {
        // Set once, here, before the socket ends, so that the session's read of its end sees it.
        let _ = ended.set((error.kind(), error.to_string()));
    }
    tls.send_close_notify();
    // What the peer doesn't take by then, it never will: the session is over either way.
    let by = closing.unwrap_or_else(|| Deadline::after(CLOSE_TIMEOUT));
    let _ = flushed(&mut tls, wire, by);
    let _ = carried.shutdown(Shutdown::Both);
    let _ = wire.shutdown();
}

/// Moves bytes both ways, waiting on whichever side has room or bytes, until the session's end of
/// its socket closes, or the peer's `close_notify` arrives and what came before it has gone to
/// the session: `Ok` then; or the error that ended the TLS connection. Once the session's end
/// hangs up, `closing` is set [`CLOSE_TIMEOUT`] on, and the moves end by then.
fn moved(
    tls: &mut Connection,
    mut wire: &Wire,
    mut carried: &UnixStream,
    closing: &mut Option<Deadline>,
) -> io::Result<()> {
    let mut chunk = vec![0; CHUNK_BYTES];
    // Plaintext for the session, of which `sent` bytes have gone to it.
    let (mut inbound, mut sent) = (Vec::new(), 0);
    let mut closed = drain(tls, &mut inbound)?;
    loop {
        if closed && inbound.is_empty() {
            return Ok(());
        }
        // The session's bytes are taken only once the records of those before them have gone,
        // and the peer's once the session has taken those before them: so each way holds at most
        // a chunk, and each waits on its own side alone.
        let (wire_read, wire_write) = (inbound.is_empty() && !closed, tls.wants_write());
        let (session_read, session_write) = (!tls.wants_write(), !inbound.is_empty());
        let mut polled = [
            watch(wire, wire_read, wire_write),
            watch(carried, session_read, session_write),
        ];
        // An entry watched for nothing is left out of the wait. The session's socket is watched
        // for its hang-up, which wakes a wait that watches it for nothing else, until it has hung
        // up: then it would wake every wait, and the wait is bounded by `closing` instead. The
        // wire's hang-up shows in the read or the write that it is watched for.
        let first = usize::from(!wire_read && !wire_write);
        let watched = session_read || session_write || closing.is_none();
        let last = if watched { 2 } else { 1 };
        wait(&mut polled[first..last], *closing)?;
        if closing.is_some_and(|by| by.remaining().is_none()) {
            return Ok(());
        }
        if watched && polled[1].revents().contains(PollFlags::HUP) {
            closing.get_or_insert_with(|| Deadline::after(CLOSE_TIMEOUT));
        }
        // Each is tried as it is watched for, whatever the wait reported, as a try that would
        // block gives way.
        if wire_write {
            match tls.write_tls(&mut wire) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(context("sending to the peer failed", &error)),
            }
        }
        if wire_read {
            closed = received(tls, wire, &mut inbound)?;
        }
        if session_write {
            match carried.write(&inbound[sent..]) {
                Ok(written) => sent += written,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                // The session has closed its end, which ends the relay as a close does.
                Err(_) => return Ok(()),
            }
            if sent == inbound.len() {
                inbound.clear();
                sent = 0;
            }
        }
        if session_read {
            match carried.read(&mut chunk) {
                Ok(0) => return Ok(()),
                Ok(read) => tls.writer().write_all(&chunk[..read])?,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => return Ok(()),
            }
        }
    }
}

/// Reads what has arrived on `wire` into `tls`, and moves its plaintext to `inbound`: whether the
/// peer's `close_notify` has arrived.
fn received(tls: &mut Connection, mut wire: &Wire, inbound: &mut Vec<u8>) -> io::Result<bool> {
    match tls.read_tls(&mut wire) {
        Ok(0) => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!(
                "{} ended without TLS's close_notify, as a connection cut short does: expected \
                 the peer to close it with one",
                wire.name()
            ),
        )),
        Ok(_) => {
            tls.process_new_packets()
                .map_err(|error| self::tls(&error))?;
            drain(tls, inbound)
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(context("receiving from the peer failed", &error)),
    }
}

/// Moves the plaintext `tls` holds to `inbound`: whether the peer's `close_notify` has arrived.
fn drain(tls: &mut Connection, inbound: &mut Vec<u8>) -> io::Result<bool> {
    match tls.reader().read_to_end(inbound) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(context("reading the peer's records failed", &error)),
    }
}

/// Sends what `tls` holds to send on `wire`, which doesn't block, by `by`.
fn flushed(tls: &mut Connection, mut wire: &Wire, by: Deadline) -> io::Result<()> {
    while tls.wants_write() && by.remaining().is_some() {
        match tls.write_tls(&mut wire) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                wait(&mut [watch(wire, false, true)], Some(by))?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn watch(fd: &impl AsFd, read: bool, write: bool) -> PollFd<'_> {
    let mut events = PollFlags::empty();
    events.set(PollFlags::IN, read);
    events.set(PollFlags::OUT, write);
    PollFd::new(fd, events)
}

/// Waits until `by`, if any, for one of `polled` to be ready as it is watched, or to hang up, and
/// fills in what each reported.
fn wait(polled: &mut [PollFd<'_>], by: Option<Deadline>) -> io::Result<()> {
    loop {
        // No timeout waits without end.
        let timeout = match by.map(Deadline::remaining) {
            None => None,
            Some(None) => return Ok(()),
            Some(Some(left)) => Some(Timespec::try_from(left).unwrap_or(Timespec {
                tv_sec: i64::MAX,
                tv_nsec: 0,
            })),
        };
        match event::poll(polled, timeout.as_ref()) {
            Ok(_) => return Ok(()),
            Err(Errno::INTR) => {}
            Err(error) => return Err(context("waiting on the connection failed", &error.into())),
        }
    }
}

fn context(what: &str, error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{what}: {error}"))
}
