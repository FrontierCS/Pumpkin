//! A stand-in for a provider's TLS edge, which ends the TLS of a connection to a sandbox's port.
//!
//! Its keys are made for these tests, and are public, so they keep nothing secret.

use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::time::Duration;
use std::{io, thread};

use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use stdx::Deadline;

/// The name the stand-in's certificate is for.
pub const NAME: &str = "localhost";

/// The root the stand-in's certificate is issued from, valid until 2126.
const ROOT: &str = "\
-----BEGIN CERTIFICATE-----
MIIBpjCCAUugAwIBAgIUTx5grUiGGmVjQ69EFZUjgmZY8HQwCgYIKoZIzj0EAwIw
HzEdMBsGA1UEAwwUUHVtcGtpbiB0ZXN0IGVkZ2UgQ0EwIBcNMjYxMTA4MTQyNDMx
WhgPMjEyNjEwMTUxNDI0MzFaMB8xHTAbBgNVBAMMFFB1bXBraW4gdGVzdCBlZGdl
IENBMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEfkcOZ+sn/PzDQhpvvtCVdbMT
WMSv7u10SRLUzzUN2VHa5IZ5kxOE3UrPWRYevdlY2F4L9qSDLqDN6As0FqxDsKNj
MGEwHQYDVR0OBBYEFDbMaVireDDygDrRmxuRrbk9tiYjMB8GA1UdIwQYMBaAFDbM
aVireDDygDrRmxuRrbk9tiYjMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQD
AgIEMAoGCCqGSM49BAMCA0kAMEYCIQDyGiaxIU3zlxukAxEzhzoC1FF72tbOOYiJ
OE4X9vsjYwIhAIo0jOTuyYjUKYSbNmkQdlSGe+918YSmcv9xEWpYHiuE
-----END CERTIFICATE-----
";

/// The stand-in's certificate, for [`NAME`], issued from its root, in PEM.
pub const CERTIFICATE: &str = "\
-----BEGIN CERTIFICATE-----
MIIBxTCCAWqgAwIBAgIUCn71eAhIceONCe/g/Ogz1uWYpWgwCgYIKoZIzj0EAwIw
HzEdMBsGA1UEAwwUUHVtcGtpbiB0ZXN0IGVkZ2UgQ0EwIBcNMjYxMTA4MTQyNDMx
WhgPMjEyNjEwMTUxNDI0MzFaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDBZMBMGByqG
SM49AgEGCCqGSM49AwEHA0IABEVLnpRScJBelKN8pUb8+dPBr+XZk990UJZPdVKE
yGo0Mgo1Y5eDg+2/xIPC8rP6h53Z+9ZSKfMPv8bgXc5ExyajgYwwgYkwFAYDVR0R
BA0wC4IJbG9jYWxob3N0MAwGA1UdEwEB/wQCMAAwDgYDVR0PAQH/BAQDAgeAMBMG
A1UdJQQMMAoGCCsGAQUFBwMBMB0GA1UdDgQWBBTMqRHxGqP6x6Qm2YVeYBZeJAS7
ODAfBgNVHSMEGDAWgBQ2zGlYq3gw8oA60Zsbka25PbYmIzAKBggqhkjOPQQDAgNJ
ADBGAiEA6pVGWMtsKAs0MgKhX2hvIr9vYCd1sRC67GvzM8n1i7wCIQDHx5XH97t2
tQQk6V12GUkvWwBzpgli2xXKzh8+xHEaRQ==
-----END CERTIFICATE-----
";

/// The private key of [`CERTIFICATE`], in PEM.
pub const KEY: &str = "\
-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgaWvrzts8xoMjdTYz
cjGZ6zMabaAQ02wWedXBfp8F+tuhRANCAARFS56UUnCQXpSjfKVG/PnTwa/l2ZPf
dFCWT3VShMhqNDIKNWOXg4Ptv8SDwvKz+oed2fvWUinzD7/G4F3ORMcm
-----END PRIVATE KEY-----
";

/// A TLS server that shows the stand-in's certificate.
///
/// # Panics
///
/// Panics if the stand-in's certificates or key, constants, can't be read.
pub fn server() -> tls::Server {
    let certificate = CertificateDer::from_pem_slice(CERTIFICATE.as_bytes()).unwrap();
    let key = PrivatePkcs8KeyDer::from_pem_slice(KEY.as_bytes()).unwrap();
    tls::Server::new(&[&certificate], key.secret_pkcs8_der()).unwrap()
}

/// A TLS client that trusts the stand-in's root alone.
///
/// # Panics
///
/// Panics if the stand-in's certificates or key, constants, can't be read.
pub fn client() -> tls::Client {
    let root = CertificateDer::from_pem_slice(ROOT.as_bytes()).unwrap();
    tls::Client::trusting(&root).unwrap()
}

/// Listens on the loopback in front of `target` as long as the process lasts; returns the port.
///
/// Each connection gets the stand-in's TLS and is copied both ways to one of its own to `target`;
/// one that can't reach `target` is closed, as an edge closes one to a sandbox not yet listening.
///
/// # Panics
///
/// Panics if it can't listen.
pub fn front(target: SocketAddr) -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || relay(stream, target));
        }
    });
    port
}

/// Ends the TLS of `stream`, and copies it both ways with a connection to `target`.
fn relay(stream: TcpStream, target: SocketAddr) {
    let Ok(secured) = server().accept(stream, Deadline::after(Duration::from_secs(60))) else {
        return;
    };
    let Ok(sandbox) = TcpStream::connect(target) else {
        return;
    };
    let (Ok(from_sandbox), Ok(to_gateway)) = (sandbox.try_clone(), secured.try_clone()) else {
        return;
    };
    let up = thread::spawn(move || {
        let _ = io::copy(&mut &secured, &mut &sandbox);
        let _ = sandbox.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut &from_sandbox, &mut &to_gateway);
    let _ = to_gateway.shutdown();
    let _ = from_sandbox.shutdown(Shutdown::Both);
    let _ = up.join();
}
