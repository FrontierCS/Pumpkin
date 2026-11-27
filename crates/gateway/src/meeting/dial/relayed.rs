//! The meeting of a companion that listens behind its sandbox's relay ([`Relayed`]).

use std::io::{self, Write as _};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, SanType,
};
use schema::ir::CompanionName;
use schema::sandbox::{Endpoint, Relayed, TlsIdentity, route};
use stdx::Deadline;

use super::{DialError, Dialer, Unreached, random_grant};
use crate::context;

// How far back a certificate is dated, so that it is valid at its check though the machine's
// clock has stepped back since its issue. Kubernetes dates its self-signed certificates an hour
// back for the same reason:
// https://github.com/kubernetes/kubernetes/blob/af46c47ce925f4c4ad5cc8d1fca46c7b77d13b38/staging/src/k8s.io/client-go/util/cert/cert.go#L95
const CLOCK_STEP: Duration = Duration::from_hours(1);

// 99991231235959Z, as seconds since the epoch. RFC 5280 assigns it to the `notAfter` of a
// certificate that has no well-defined expiration date:
// https://www.rfc-editor.org/rfc/rfc5280#section-4.1.2.5
const NO_EXPIRATION: Duration = Duration::from_secs(253_402_300_799);

impl Dialer {
    /// Issues the grant and the TLS identity of `companion`, in the [`Relayed`] that its
    /// `sandboxd` is given. The [`Unreached`] takes the endpoint of its sandbox's relay.
    pub fn relayed(&self, companion: &CompanionName) -> io::Result<(Relayed, Unreached)> {
        let grant = random_grant()?;
        let (identity, within) = Within::issue(companion)?;
        let unreached = Unreached {
            client: self.client.clone(),
            grant: grant.clone(),
            within: Some(within),
        };
        Ok((Relayed { grant, identity }, unreached))
    }
}

// What reaches one companion within a connection to its sandbox's relay.
#[derive(Debug)]
pub(super) struct Within {
    companion: CompanionName,
    client: tls::Client,
}

impl Within {
    // Makes a key and its certificate for `companion`, and what reaches the companion under them.
    // Nothing else makes a `Within`, so its client's one root is a certificate issued here.
    fn issue(companion: &CompanionName) -> io::Result<(TlsIdentity, Self)> {
        let failed = |error: rcgen::Error| {
            io::Error::other(format!(
                "a certificate for companion `{companion}` can't be issued: {error}"
            ))
        };
        // An ECDSA key on P-256, of the kernel's random bytes, which signing draws on too.
        let key = KeyPair::generate().map_err(failed)?;
        let certificate = certificate(companion).map_err(failed)?;
        let certificate = certificate.self_signed(&key).map_err(failed)?;
        let within = Self {
            companion: companion.clone(),
            client: tls::Client::trusting(certificate.der())?,
        };
        // The key leaves in the identity. `key`, its other holder, drops here, uncleared.
        let identity = TlsIdentity::new(certificate.der().to_vec(), key.serialize_der());
        Ok((identity, within))
    }

    pub(super) fn dialed(&self, relay: &Endpoint) -> String {
        let companion = &self.companion;
        format!("companion `{companion}` through its sandbox's relay at {relay}")
    }

    // Names the companion on `outer`, a connection to the relay at `relay`, and runs the
    // companion's TLS within it, by `by`.
    pub(super) fn connect(
        &self,
        outer: tls::Stream,
        relay: &Endpoint,
        by: Deadline,
    ) -> Result<tls::Stream, DialError> {
        let Self { companion, client } = self;
        // The route alone goes to the relay as it is. A deadline that has passed sends nothing.
        let route = route(companion);
        let sent = match by.remaining() {
            Some(left) => outer
                .set_write_timeout(Some(left))
                .and_then(|()| (&outer).write_all(route.as_bytes())),
            None => Err(io::ErrorKind::TimedOut.into()),
        };
        let within = sent.and_then(|()| client.connect_within(outer, companion.as_str(), by));
        within.map_err(|error| match error.kind() {
            // TLS's own failures, a certificate that the issued key didn't sign among them:
            // another dial would meet the same.
            io::ErrorKind::InvalidData => {
                let what = format_args!(
                    "the gateway can't trust companion `{companion}` behind the relay at {relay}"
                );
                DialError::Never(context(what, &error))
            }
            // How a connection that has ended fails a read, or a write once its end is known.
            // Which comes first is a race, so the message names neither. A later dial may find
            // the companion, which may not listen yet.
            io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset => DialError::Again(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                format!(
                    "the relay at {relay} closed the connection before the TLS handshake with \
                     companion `{companion}` was done"
                ),
            )),
            _ => {
                let what = format_args!("companion `{companion}` behind the relay at {relay}");
                DialError::Again(context(what, &error))
            }
        })
    }
}

// The certificate of `companion`, yet to be signed.
fn certificate(companion: &CompanionName) -> Result<CertificateParams, rcgen::Error> {
    let no_expiration = UNIX_EPOCH + NO_EXPIRATION;
    let start = SystemTime::now().checked_sub(CLOCK_STEP);
    // The fields that aren't set here keep rcgen's defaults. Those add no extension but the
    // key's identifier, which goes with `is_ca`.
    let mut params = CertificateParams::default();
    // Clamped to the dates that a certificate holds, past which the conversion would panic.
    params.not_before = (start.unwrap_or(UNIX_EPOCH))
        .clamp(UNIX_EPOCH, no_expiration)
        .into();
    // No expiration: only its registration's client trusts the certificate, and the session's
    // opening spends that client. An end would only fail an accept that outlasts it.
    params.not_after = no_expiration.into();
    // The name that the client checks is the alternative name. The subject says it to a reader.
    params.subject_alt_names = vec![SanType::DnsName(companion.as_str().try_into()?)];
    params.distinguished_name = DistinguishedName::new();
    (params.distinguished_name).push(DnType::CommonName, companion.as_str());
    // No CA's, which rustls refuses as a server's own certificate. The certificate says so.
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    Ok(params)
}
