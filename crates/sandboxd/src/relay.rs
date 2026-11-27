//! The relay in a sandbox's `main`, through which the gateway reaches the sandbox's companions
//! ([`schema::sandbox::Relayed`]).
//!
//! A connection first says its [route](schema::sandbox::route), a companion's name. The relay
//! then connects to that companion by its name, at [`RELAYED_PORT`], and carries bytes both ways
//! until either connection ends, which ends both. It says on standard error why it closed a
//! connection.
//!
//! The relay is no boundary. The role's code can end it or take its place, so nothing trusts it.
//!
//! What its peers make it hold is bounded. One thread takes the connections and reads each route
//! in turn, as a `sandboxd` that listens greets one peer at a time. It connects only to the
//! companions it was given, and carries at most one connection a companion at a time.

use std::collections::BTreeMap;
use std::io;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use schema::ir::CompanionName;
use schema::sandbox::{HELLO_TIMEOUT, RELAYED_PORT, read_route};
use stdx::{Context, Deadline};

use crate::session::TOLD_PEERS_COUNT;
use crate::until::Until;
use crate::{accept, print_line};

// A route is the first that its connection says, as a hello is, so it waits for the network
// alone.
const ROUTE_TIMEOUT: Duration = HELLO_TIMEOUT;

// How long a companion has to take the relay's connection: as long as a hello gets, as it waits
// for the network alone too.
const CONNECT_TIMEOUT: Duration = HELLO_TIMEOUT;

// As many as a `sandboxd` that listens tells of (`TOLD_PEERS_COUNT`). Any container of the
// sandbox may reach the relay, as often as it likes.
const TOLD_COUNT: usize = TOLD_PEERS_COUNT;

/// Listens at `address`, and from then on relays each connection there to the companion that
/// its route names, one of `relayed`. It relays as long as the process lasts.
pub(crate) fn serve(address: SocketAddr, relayed: Vec<CompanionName>) -> Result<(), String> {
    let listener = TcpListener::bind(address)
        .context(format_args!("listening as a relay at {address} failed"))?;
    let mut relay = Relay {
        carried: relayed.into_iter().map(|name| (name, None)).collect(),
        told: Arc::new(Told::default()),
    };
    thread::Builder::new()
        .name("relay".to_owned())
        .spawn(move || relay.take(&listener))
        .context("the thread that takes the relay's connections didn't start")?;
    Ok(())
}

struct Relay {
    /// Each companion that the relay carries to, with the thread that carries its connection
    /// once one has started. A companion whose thread hasn't finished has a connection in flight.
    carried: BTreeMap<CompanionName, Option<JoinHandle<()>>>,
    told: Arc<Told>,
}

impl Relay {
    fn take(&mut self, listener: &TcpListener) -> ! {
        accept::each(
            || listener.accept(),
            |(gateway, peer)| match self.routed(&gateway) {
                Ok(companion) => self.carry(gateway, peer, &companion),
                // Told before `gateway` drops, which closes it.
                Err(why) => self.told.tell(peer, &why),
            },
        )
    }

    /// The companion that the route of `gateway` names, if the relay is to carry the connection
    /// to it. Fails, saying why, for a connection that the relay doesn't carry.
    fn routed(&self, gateway: &TcpStream) -> Result<CompanionName, String> {
        stdx::net::set_options(gateway).context("setting its connection's options failed")?;
        let companion = routed(gateway)?;
        let Some(carrying) = self.carried.get(&companion) else {
            return Err(format!(
                "its route names `{companion}`, which this relay carries nothing to: expected a \
                 companion that --relayed names"
            ));
        };
        if (carrying.as_ref()).is_some_and(|thread| !thread.is_finished()) {
            return Err(format!(
                "a connection to companion `{companion}` is in flight: expected one at a time, \
                 as the gateway opens one session with it"
            ));
        }
        Ok(companion)
    }

    /// Starts the thread that carries `gateway`, a connection from `peer`, to `companion`, which
    /// has no connection in flight.
    fn carry(&mut self, gateway: TcpStream, peer: SocketAddr, companion: &CompanionName) {
        let (told, name) = (Arc::clone(&self.told), companion.clone());
        let carry = move || {
            // Told before `gateway` drops, which closes it.
            if let Err(why) = carry(&gateway, &name) {
                told.tell(peer, &why);
            }
        };
        let thread = thread::Builder::new().name("relayed".to_owned());
        match thread.spawn(carry) {
            Ok(thread) => {
                self.carried.insert(companion.clone(), Some(thread));
            }
            // A thread that can't start has dropped `carry`, and so closed the connection.
            Err(error) => self.told.tell(
                peer,
                &format!(
                    "the thread that carries its connection to companion `{companion}` didn't \
                     start: {error}"
                ),
            ),
        }
    }
}

/// The companion that the route of `gateway` names, read within `ROUTE_TIMEOUT`.
fn routed(gateway: &TcpStream) -> Result<CompanionName, String> {
    let mut bounded = Until::new(gateway, Deadline::after(ROUTE_TIMEOUT));
    let companion = read_route(&mut bounded).map_err(|error| match error.kind() {
        io::ErrorKind::TimedOut => format!(
            "it sent no route within {} s: expected a companion's name and a line feed, as the \
             connection opens",
            ROUTE_TIMEOUT.as_secs()
        ),
        _ => error.to_string(),
    })?;
    let lifted = gateway.set_read_timeout(None);
    lifted.context("lifting the bound on its connection's reads failed")?;
    Ok(companion)
}

/// Connects to `companion`, and carries bytes between it and `gateway` until either connection
/// ends. Both have ended, both ways, once it returns. Fails if it carried nothing.
fn carry(gateway: &TcpStream, companion: &CompanionName) -> Result<(), String> {
    let reached = reach(companion).context(format_args!(
        "companion `{companion}` wasn't reached at {companion}:{RELAYED_PORT}"
    ))?;
    thread::scope(|scope| {
        let back = thread::Builder::new().name("relayed-back".to_owned());
        let back = back.spawn_scoped(scope, || copy(&reached, gateway));
        back.context(format_args!(
            "the thread that carries companion `{companion}`'s bytes back didn't start"
        ))?;
        copy(gateway, &reached);
        Ok(())
    })
}

/// A connection to `companion` by its name, at the first of its addresses that takes one, within
/// `CONNECT_TIMEOUT` in all.
fn reach(companion: &CompanionName) -> Result<TcpStream, String> {
    // The resolver's own timeouts and attempts (`resolv.conf(5)`) bound the lookup.
    let addresses = (companion.as_str(), RELAYED_PORT).to_socket_addrs();
    let addresses = addresses.context("its name didn't resolve")?;
    let by = Deadline::after(CONNECT_TIMEOUT);
    let mut failed = "its name resolved to no address".to_owned();
    for address in addresses {
        let Some(left) = by.remaining() else {
            break;
        };
        match TcpStream::connect_timeout(&address, left) {
            Ok(reached) => {
                stdx::net::set_options(&reached)
                    .context("setting the options of the connection to it failed")?;
                return Ok(reached);
            }
            Err(error) => failed = format!("connecting to {address} failed: {error}"),
        }
    }
    Err(failed)
}

/// Copies `from` to `to` until `from` ends or either fails, then ends both connections, both
/// ways. That ends the copy the other way too, whether it waits in a read or in a write.
// Go's reverse proxy ends a connection it has upgraded alike, once either copy returns:
// https://github.com/golang/go/blob/f21be2fdc6f1becdbed1592ea0b245cdeedc5ac8/src/net/http/httputil/reverseproxy.go#L595-L600
// What it carries is TLS, which marks its own end, so nothing rests on TCP's half-close.
fn copy(mut from: &TcpStream, mut to: &TcpStream) {
    let _ = io::copy(&mut from, &mut to);
    // A connection that its peer has ended refuses the shutdown, which is as good as done.
    let _ = from.shutdown(Shutdown::Both);
    let _ = to.shutdown(Shutdown::Both);
}

/// How many closed connections the relay has told of. The thread that takes connections and
/// those that carry them share it, so that they tell of `TOLD_COUNT` between them.
#[derive(Default)]
struct Told(AtomicUsize);

impl Told {
    /// Says on standard error `why` the relay closed the connection from `peer`, for the first
    /// `TOLD_COUNT` of them.
    fn tell(&self, peer: SocketAddr, why: &str) {
        let told = self.0.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        if told <= TOLD_COUNT {
            let line = format_args!("sandboxd: the relay closed the connection from {peer}: {why}");
            print_line(io::stderr(), line);
        }
        if told == TOLD_COUNT {
            let line = format_args!(
                "sandboxd: telling of no more connections that the relay closes, having told of \
                 {TOLD_COUNT}"
            );
            print_line(io::stderr(), line);
        }
    }
}
