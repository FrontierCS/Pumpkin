//! The meeting of a sandbox whose `sandboxd` listens: the gateway dials it, at the endpoint that
//! its create gives.

use std::fs::File;
use std::io::{self, Read};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;
use std::{fmt, thread};

use schema::sandbox::{
    Endpoint, HELLO_TIMEOUT, Hello, Listening, REGISTRATION_GRANT_BYTES, RegistrationGrant,
};
use stdx::Deadline;
use stdx::time::{Clock, Machine};

use super::{Meeting, Met, POLL, Registration, read_hello, refuse, unstopped};
use crate::{Bound, Connection, context};

const RANDOM: &str = "/dev/urandom";

// A dial that fails is tried again after this wait, which doubles up to `REDIAL_MOST`. It starts
// as gRPC's connection backoff does, at its INITIAL_BACKOFF of 1 s:
// https://github.com/grpc/grpc/blob/785ff9afd6410c535a44690e7cca1f81228d8851/doc/connection-backoff.md#L36-L38
// A sandbox's `sandboxd` listens seconds after its create, and the provider's edge may refuse a
// dial until then.
const REDIAL_FIRST: Duration = Duration::from_secs(1);

// Shorter than gRPC's 120 s, as a start's window is minutes, of which a wait spends little.
const REDIAL_MOST: Duration = Duration::from_secs(15);

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
    /// [`Unreached`] that takes the sandbox's endpoint, which is known only once the sandbox is
    /// created, and so after the grant is given.
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
pub(super) struct Dialed {
    client: tls::Client,
    pub(super) grant: RegistrationGrant,
    endpoint: Endpoint,
}

const LISTENING: &str = "its sandboxd to listen there";

impl Dialed {
    fn awaited(&self) -> impl fmt::Display {
        format!("the gateway reached the sandbox at {}", self.endpoint)
    }

    pub(super) fn connected(
        &self,
        by: Bound<'_, impl Clock>,
        stopped: &dyn Fn() -> bool,
    ) -> io::Result<Met> {
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
            let dial = Bound {
                clock: &Machine,
                by: Machine.now() + left.min(HELLO_TIMEOUT),
            };
            match self.dial(dial, stopped) {
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

    fn dial(&self, by: Bound<'_, Machine>, stopped: &dyn Fn() -> bool) -> Result<Met, DialError> {
        let endpoint = &self.endpoint;
        let deadline = by.deadline();
        let stream = self.connect(deadline, stopped)?;
        unstopped(stopped, self.awaited(), LISTENING).map_err(DialError::Never)?;
        let secured = self.client.connect(stream, endpoint.host(), deadline);
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
        match read_hello(&connection, by, stopped) {
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
