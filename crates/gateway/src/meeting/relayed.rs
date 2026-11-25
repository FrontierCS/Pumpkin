//! How the gateway dials a companion that listens behind its sandbox's relay ([`Relayed`]):
//! through TLS within the relay's own, to the companion's certificate, which the dialer issues for
//! it alone.

use std::io::{self, Write as _};

use schema::ir::CompanionName;
use schema::sandbox::{Endpoint, Relayed, TlsIdentity};
use stdx::Deadline;

use super::{DialError, Dialer, Unreached, random_grant, untrusted};

/// A companion behind its sandbox's relay, and the client that trusts its certificate alone.
#[derive(Debug)]
pub(super) struct Within {
    companion: CompanionName,
    client: tls::Client,
}

impl Dialer {
    /// Issues the grant and the TLS identity of `companion`, a companion of a sandbox whose relay
    /// it listens behind, in the [`Relayed`] that its `sandboxd` is given, and an [`Unreached`]
    /// that takes the relay's endpoint, as [`issue`](Self::issue) does.
    pub fn relayed(&self, companion: &CompanionName) -> io::Result<(Relayed, Unreached)> {
        let grant = random_grant()?;
        let tls::Issued { certificate, key } = tls::issue(companion.as_str())?;
        let within = Within {
            companion: companion.clone(),
            client: tls::Client::trusting(&certificate)?,
        };
        let unreached = Unreached {
            client: self.client.clone(),
            grant: grant.clone(),
            within: Some(within),
        };
        let identity = TlsIdentity { certificate, key };
        Ok((Relayed { grant, identity }, unreached))
    }
}

impl Within {
    pub(super) fn awaited(&self, relay: &Endpoint) -> String {
        let companion = &self.companion;
        format!(
            "the gateway reached companion `{companion}` through its sandbox's relay at {relay}"
        )
    }

    /// Sends the companion's route on `secured`, a connection to the relay at `relay`, and runs
    /// the companion's TLS within it, by `by`. The relay closes a connection to a companion that
    /// doesn't listen yet, which another dial may find.
    pub(super) fn connect(
        &self,
        secured: tls::Stream,
        relay: &Endpoint,
        by: Deadline,
    ) -> Result<tls::Stream, DialError> {
        let Self { companion, client } = self;
        // A route fits in the socket's buffer, so its write never waits on the peer.
        let route = schema::sandbox::route(companion);
        (&secured)
            .write_all(route.as_bytes())
            .map_err(DialError::Again)?;
        let within = client.connect_within(secured, companion.as_str(), by);
        within.map_err(|error| {
            let what = format_args!(
                "the gateway can't trust companion `{companion}` behind the relay at {relay}"
            );
            untrusted(error, &what)
        })
    }
}
