//! The links between this container and the others of its pod ([`schema::ir::Link`]), which the
//! gateway carries over the session ([the protocol](schema::sandbox)).
//!
//! For each link from this container (`--link`), `sandboxd` listens at the link's port on the
//! loopback address where the pod's containers reach the other container, and asks the gateway
//! for a link for each connection it takes there. For each link that the gateway opens
//! to this container, it connects to the port on `127.0.0.1`, where the container's own service
//! listens. Either way, it then carries the connection's bytes both ways, within each way's window,
//! until both ways end: a thread of the link's own writes what arrives to the connection, and
//! grants it back once written, and another reads what the connection sends, while the window
//! lets it. The session's reader only hands each frame over, so no link holds it up.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::str::FromStr;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;
use std::{fmt, thread};

use schema::ir::{LinkPort, Member};
use schema::sandbox::{FromSandbox, HELLO_TIMEOUT, LINK_WINDOW_BYTES};
use stdx::{Context, unpoisoned};

use crate::session::Sender;

// How long a link's listener waits before it takes a connection again, after taking one failed, as
// when the process has no file descriptor to spare.
const RETAKE: Duration = Duration::from_millis(50);

// The most bytes a link reads from its connection at once, into one frame.
const READ_BYTES: usize = 64 << 10;

/// A link from this container, as `--link` gives it: `<to>:<port>@<address>`, the container that
/// it joins and its port, and the loopback address where `sandboxd` takes its connections.
#[derive(Debug)]
pub(crate) struct Link {
    to: Member,
    port: LinkPort,
    address: Ipv4Addr,
}

impl FromStr for Link {
    type Err = String;

    fn from_str(link: &str) -> Result<Self, Self::Err> {
        let refused = |why: String| {
            format!(
                "`{link}` is no link: {why}: give `<to>:<port>@<address>`, such as \
                 `main:8000@127.0.0.2`"
            )
        };
        let Some((joined, address)) = link.split_once('@') else {
            return Err(refused("it has no `@`".to_owned()));
        };
        let Some((to, port)) = joined.split_once(':') else {
            return Err(refused("it has no `:`".to_owned()));
        };
        let to = Member::try_from(to.to_owned()).map_err(refused)?;
        let port = port
            .parse::<u16>()
            .map_err(|error| refused(error.to_string()))?;
        let port = LinkPort::try_from(port).map_err(refused)?;
        let address = address
            .parse::<Ipv4Addr>()
            .map_err(|error| refused(error.to_string()))?;
        if !address.is_loopback() {
            return Err(refused(format!(
                "{address} is no loopback address, which only this container reaches"
            )));
        }
        Ok(Self { to, port, address })
    }
}

/// The session's links: the connections taken whose links the gateway has yet to open or refuse,
/// and the links open, each by its number.
#[derive(Clone, Default)]
pub(crate) struct Links(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    next: u32,
    asked: BTreeMap<u32, TcpStream>,
    open: BTreeMap<u32, Arc<Carried>>,
}

/// What a link's two threads and the session's reader share of it.
struct Carried {
    way: Mutex<Way>,
    changed: Condvar,
}

#[derive(Default)]
struct Way {
    // What arrived and isn't yet written to the connection.
    arrived: VecDeque<Vec<u8>>,
    // Whether the gateway has ended the link's way to the connection.
    ended: bool,
    // The bytes that `sandboxd` may send of the link before the gateway grants more back.
    window: u32,
    // The link's threads that haven't finished: it closes once neither is left.
    left: u8,
}

impl Links {
    /// Listens for each of `links`' connections, and asks the gateway for a link for each one
    /// taken, on threads of its own as long as the process lasts. Fails if an address can't be
    /// listened at, as when the container's own code listens there already.
    pub(crate) fn serve(&self, sender: &Sender, links: Vec<Link>) -> Result<(), String> {
        for Link { to, port, address } in links {
            let at = SocketAddr::from((address, port.get()));
            let listener = TcpListener::bind(at).context(format_args!(
                "listening for the link to `{to}` at {at} failed"
            ))?;
            let (links, sender) = (self.clone(), sender.clone());
            let starting = format!("starting the link to `{to}` at {at} failed");
            thread::Builder::new()
                .name("link".to_owned())
                .spawn(move || links.take(&listener, &sender, (&to, port)))
                .context(starting)?;
        }
        Ok(())
    }

    fn take(&self, listener: &TcpListener, sender: &Sender, (to, port): (&Member, LinkPort)) -> ! {
        loop {
            let Ok((stream, _)) = listener.accept() else {
                thread::sleep(RETAKE);
                continue;
            };
            let request = {
                let mut state = self.state();
                let request = state.next;
                state.next = request.wrapping_add(1);
                state.asked.insert(request, stream);
                request
            };
            let ask = FromSandbox::LinkOpen {
                request,
                to: to.clone(),
                port,
            };
            // A failed send has failed the session, which then ends `sandboxd`.
            let _ = sender.send(&ask, &[]);
        }
    }

    /// Carries the connection that request `request` asked for over link `link`, which the gateway
    /// opened.
    pub(crate) fn opened(&self, sender: &Sender, request: u32, link: u32) {
        let asked = self.state().asked.remove(&request);
        self.carry(sender, link, move || asked);
    }

    /// Closes the connection whose link the gateway refused, saying why on standard error.
    pub(crate) fn refused(&self, request: u32, message: &str) {
        drop(self.state().asked.remove(&request));
        let line = format_args!("sandboxd: the gateway refused a link: {message}");
        crate::print_line(io::stderr(), line);
    }

    /// Connects link `link`, which the gateway opened to this container, to the service at `port`
    /// on `127.0.0.1`, and carries it.
    pub(crate) fn connect(&self, sender: &Sender, link: u32, port: LinkPort) {
        let at = SocketAddr::from((Ipv4Addr::LOCALHOST, port.get()));
        self.carry(sender, link, move || {
            TcpStream::connect_timeout(&at, HELLO_TIMEOUT).ok()
        });
    }

    // Opens link `link` and carries it over the connection that `connection` gives, on threads of
    // the link's own: one that writes what arrives, which first takes the connection, and one that
    // it starts to send what the connection sends. Without a connection, or the thread that sends,
    // it ends the link's way to the gateway and drops what arrives, granting it back.
    fn carry(
        &self,
        sender: &Sender,
        link: u32,
        connection: impl FnOnce() -> Option<TcpStream> + Send + 'static,
    ) {
        let carried = Arc::new(Carried {
            way: Mutex::new(Way {
                window: LINK_WINDOW_BYTES,
                left: 2,
                ..Way::default()
            }),
            changed: Condvar::new(),
        });
        // Open from now, so that what arrives while it connects waits for it.
        self.state().open.insert(link, Arc::clone(&carried));
        let (links, sending) = (self.clone(), sender.clone());
        let spawned = thread::Builder::new()
            .name("link-in".to_owned())
            .spawn(move || {
                let stream = connection().filter(|stream| stdx::net::set_options(stream).is_ok());
                let reading = stream.as_ref().and_then(|stream| stream.try_clone().ok());
                let started = reading.and_then(|reading| {
                    let (links, sender, carried) =
                        (links.clone(), sending.clone(), Arc::clone(&carried));
                    let spawned = thread::Builder::new()
                        .name("link-out".to_owned())
                        .spawn(move || links.send_out(&sender, link, &carried, reading));
                    spawned.ok()
                });
                if started.is_none() {
                    let _ = sending.send(&FromSandbox::LinkEnd { link }, &[]);
                    links.finished(link, &carried);
                }
                links.write_in(&sending, link, &carried, stream);
            });
        // No thread starts while the sandbox's processes fill its limit: the link then takes
        // nothing more, and its other end waits once its window is spent.
        if spawned.is_err() {
            let _ = sender.send(&FromSandbox::LinkEnd { link }, &[]);
            self.state().open.remove(&link);
        }
    }

    // Sends what the connection sends, while the window lets it, then ends the link's way out.
    fn send_out(&self, sender: &Sender, link: u32, carried: &Carried, mut stream: TcpStream) {
        let mut buffer = vec![0; READ_BYTES];
        loop {
            let window = {
                let way = carried.wait(|way| way.window > 0);
                way.window
            };
            let most = buffer.len().min(window as usize);
            let read = match stream.read(&mut buffer[..most]) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            carried.lock().window -= bytes(read);
            if sender
                .send(&FromSandbox::LinkData { link }, &buffer[..read])
                .is_err()
            {
                return;
            }
        }
        let _ = sender.send(&FromSandbox::LinkEnd { link }, &[]);
        self.finished(link, carried);
    }

    // Writes what arrives to the connection, granting each part back once written, until the
    // gateway ends the link's way in; then ends the connection's way out. Once a write fails, or
    // without a connection, it drops what arrives, granting it back still.
    fn write_in(
        &self,
        sender: &Sender,
        link: u32,
        carried: &Carried,
        mut stream: Option<TcpStream>,
    ) {
        loop {
            let part = {
                let mut way = carried.wait(|way| !way.arrived.is_empty() || way.ended);
                way.arrived.pop_front()
            };
            let Some(part) = part else {
                break;
            };
            if let Some(connection) = &mut stream
                && connection.write_all(&part).is_err()
            {
                stream = None;
            }
            let bytes = bytes(part.len());
            if sender
                .send(&FromSandbox::LinkCredit { link, bytes }, &[])
                .is_err()
            {
                return;
            }
        }
        if let Some(connection) = stream {
            let _ = connection.shutdown(Shutdown::Write);
        }
        self.finished(link, carried);
    }

    fn finished(&self, link: u32, carried: &Carried) {
        let mut way = carried.lock();
        way.left -= 1;
        if way.left == 0 {
            drop(way);
            self.state().open.remove(&link);
        }
    }

    /// Hands bytes of link `link` to its thread that writes them.
    pub(crate) fn data(&self, link: u32, bytes: Vec<u8>) {
        self.change(link, |way| way.arrived.push_back(bytes));
    }

    /// Grants back `bytes` of link `link`, which it may send again.
    pub(crate) fn credit(&self, link: u32, bytes: u32) {
        self.change(link, |way| way.window += bytes);
    }

    /// Ends link `link`'s way in, once what arrived before is written.
    pub(crate) fn end(&self, link: u32) {
        self.change(link, |way| way.ended = true);
    }

    fn change(&self, link: u32, change: impl FnOnce(&mut Way)) {
        // A link that has closed takes nothing more: the gateway grants back the last bytes sent
        // of it, which may come after its end.
        let Some(carried) = self.state().open.get(&link).cloned() else {
            return;
        };
        change(&mut carried.lock());
        carried.changed.notify_all();
    }

    fn state(&self) -> MutexGuard<'_, State> {
        unpoisoned(self.0.lock())
    }
}

// The length of a part of a link, which is at most its window, a `u32`.
fn bytes(len: usize) -> u32 {
    let Ok(bytes) = u32::try_from(len) else {
        unreachable!(
            "a part of a link of {len} bytes overflows a u32, though it is at most the link's \
             window, `LINK_WINDOW_BYTES`, a u32"
        )
    };
    bytes
}

impl Carried {
    fn lock(&self) -> MutexGuard<'_, Way> {
        unpoisoned(self.way.lock())
    }

    fn wait(&self, ready: impl Fn(&Way) -> bool) -> MutexGuard<'_, Way> {
        let mut way = self.lock();
        while !ready(&way) {
            way = unpoisoned(self.changed.wait(way));
        }
        way
    }
}

impl fmt::Debug for Links {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Links").finish_non_exhaustive()
    }
}
