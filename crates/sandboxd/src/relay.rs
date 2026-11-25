//! The relay through which the gateway reaches the companions of a sandbox whose `sandboxd` is its
//! own container's ([`schema::sandbox::Relayed`]).
//!
//! Each connection starts with a route, a companion's name and a line feed, read within
//! [`HELLO_TIMEOUT`]; the relay then carries the rest both ways between it and a connection to
//! that companion, by its name, at the port where the companions listen, until either ends. A
//! connection whose route names no companion, or none that the relay reaches, it closes, so the
//! gateway dials again. It carries a TLS session that it can't read: the relay is no boundary, as
//! `sandboxd` is none.

use std::io::{self, Read as _};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs as _};
use std::thread;
use std::time::Duration;

use schema::ir::CompanionName;
use schema::sandbox::{HELLO_TIMEOUT, MAX_ROUTE_BYTES};

// How long the relay waits before it takes a connection again, after taking one failed, as when
// the process has no file descriptor to spare.
const RETAKE: Duration = Duration::from_millis(50);

/// Listens at `address`, and relays each connection there to the companion its route names, at
/// `port`, each on threads of its own, as long as the process lasts.
pub(crate) fn serve(address: SocketAddr, port: u16) -> io::Result<()> {
    let listener = TcpListener::bind(address)?;
    thread::Builder::new()
        .name("relay".to_owned())
        .spawn(move || take(&listener, port))?;
    Ok(())
}

fn take(listener: &TcpListener, port: u16) -> ! {
    loop {
        match listener.accept() {
            // A connection that no thread can carry closes as its closure drops, and its peer may
            // dial again.
            Ok((stream, _)) => {
                let _ = (thread::Builder::new())
                    .name("relayed".to_owned())
                    .spawn(move || carry(&stream, port));
            }
            Err(_) => thread::sleep(RETAKE),
        }
    }
}

/// Reads `gateway`'s route, and carries the rest to the companion it names, at `port`; or closes
/// it, as it does once either end has ended.
fn carry(gateway: &TcpStream, port: u16) {
    let Some(companion) = routed(gateway) else {
        return;
    };
    let Some(companion) = reached(&companion, port) else {
        return;
    };
    splice(gateway, &companion);
}

/// Copies each of `one` and `other` to the other, the second way on a thread of its own, until
/// both ways have ended; or until this one has, if no thread can start.
fn splice(one: &TcpStream, other: &TcpStream) {
    let (Ok(up), Ok(down)) = (one.try_clone(), other.try_clone()) else {
        return;
    };
    let carried = thread::Builder::new()
        .name("spliced".to_owned())
        .spawn(move || copy(&up, &down));
    copy(other, one);
    if let Ok(carried) = carried {
        let _ = carried.join();
    }
}

/// The companion that `gateway`'s route names, read byte by byte, so that nothing past it is
/// taken, within [`HELLO_TIMEOUT`]; or none, if it names none by then.
fn routed(gateway: &TcpStream) -> Option<CompanionName> {
    stdx::net::set_options(gateway).ok()?;
    gateway.set_read_timeout(Some(HELLO_TIMEOUT)).ok()?;
    let mut route = Vec::new();
    let mut byte = [0];
    while route.len() < MAX_ROUTE_BYTES {
        (&*gateway).read_exact(&mut byte).ok()?;
        if byte == *b"\n" {
            gateway.set_read_timeout(None).ok()?;
            let name = String::from_utf8(route).ok()?;
            return CompanionName::try_from(name).ok();
        }
        route.push(byte[0]);
    }
    None
}

/// A connection to `companion` at `port`, at the first of its addresses that takes one within
/// [`HELLO_TIMEOUT`], or none.
fn reached(companion: &CompanionName, port: u16) -> Option<TcpStream> {
    let addresses = (companion.as_str(), port).to_socket_addrs().ok()?;
    let mut reached =
        addresses.filter_map(|address| TcpStream::connect_timeout(&address, HELLO_TIMEOUT).ok());
    let stream = reached.next()?;
    stdx::net::set_options(&stream).ok()?;
    Some(stream)
}

/// Copies `from` to `to` until `from` ends, or either fails; then ends `to`'s writes and `from`'s
/// reads, so that the other way ends as its peer sees this one end.
fn copy(from: &TcpStream, to: &TcpStream) {
    let _ = io::copy(&mut &*from, &mut &*to);
    let _ = to.shutdown(Shutdown::Write);
    let _ = from.shutdown(Shutdown::Read);
}
