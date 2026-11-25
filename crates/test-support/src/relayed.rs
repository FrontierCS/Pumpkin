//! Stand-ins for what `sandboxd` does for a companion behind its sandbox's relay. One is the
//! relay, in the sandbox's `main`, and one the companion's TLS server.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use schema::ir::CompanionName;
use schema::sandbox::{TlsIdentity, read_route};

use crate::HUNG;

/// A stand-in relay, which listens at `address`, on the loopback, as long as the process lasts.
pub struct Relay {
    pub address: SocketAddr,
    ended: Receiver<Carried>,
}

/// What one connection to a [`Relay`] carried after its route, each way: nothing if the relay
/// closed it at its route.
#[derive(Default)]
pub struct Carried {
    pub to_companion: Vec<u8>,
    pub to_gateway: Vec<u8>,
}

/// Listens as a sandbox's relay does, for the companions that listen at `companions`.
///
/// Each connection says its route, and is then copied both ways with a connection of its own to
/// the companion that the route names. One whose route names no companion of `companions`, or
/// one that doesn't listen, is closed.
pub fn relay(companions: BTreeMap<CompanionName, SocketAddr>) -> Relay {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (carried, ended) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (companions, carried) = (companions.clone(), carried.clone());
            thread::spawn(move || {
                // Nothing receives it once the test has ended.
                let _ = carried.send(carry(&stream, &companions));
            });
        }
    });
    Relay { address, ended }
}

impl Relay {
    /// Waits for a connection to end both ways, and returns what it carried: each call, another
    /// connection's.
    pub fn ended(&self) -> Carried {
        self.ended.recv_timeout(HUNG).unwrap()
    }
}

fn carry(mut gateway: &TcpStream, companions: &BTreeMap<CompanionName, SocketAddr>) -> Carried {
    let address = read_route(&mut gateway)
        .ok()
        .and_then(|companion| companions.get(&companion));
    let Some(companion) = address.and_then(|address| TcpStream::connect(address).ok()) else {
        return Carried::default();
    };
    thread::scope(|scope| {
        let up = scope.spawn(|| copied(gateway, &companion));
        let to_gateway = copied(&companion, gateway);
        Carried {
            to_companion: up.join().unwrap(),
            to_gateway,
        }
    })
}

/// Copies `from` to `to` until `from` ends, then ends what `to` is sent: what it copied.
fn copied(mut from: &TcpStream, mut to: &TcpStream) -> Vec<u8> {
    let mut copied = Vec::new();
    let mut chunk = [0; 16 << 10];
    while let Ok(read @ 1..) = from.read(&mut chunk) {
        copied.extend(&chunk[..read]);
        if to.write_all(&chunk[..read]).is_err() {
            break;
        }
    }
    let _ = to.shutdown(Shutdown::Write);
    copied
}

/// The TLS server of a companion that was given `identity`.
///
/// It reads the identity as the companion's `sandboxd` is to: from the digits that the two
/// variables of its environment hold.
pub fn server(identity: &TlsIdentity) -> tls::Server {
    let (certificate, key) = (identity.certificate_digits(), identity.key_digits());
    let identity = TlsIdentity::from_digits(&certificate, &key).unwrap();
    tls::Server::new(&[identity.certificate()], identity.key()).unwrap()
}
