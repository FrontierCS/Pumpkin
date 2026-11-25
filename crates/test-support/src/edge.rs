//! A stand-in for a provider's TLS edge, which ends the TLS of a connection to a sandbox's port.
//!
//! Its keys are made for these tests, and are public, so they keep nothing secret.

use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::{io, thread};

use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use stdx::Deadline;

use crate::HUNG;

/// The name the stand-in's certificate is for.
pub const NAME: &str = "localhost";

/// The name of [`other_server`]'s certificate: no edge's, and one that a companion may have.
pub const OTHER_NAME: &str = "companion";

/// The root the stand-in's certificates are issued from, valid until 2126.
const ROOT: &str = "\
-----BEGIN CERTIFICATE-----
MIIBpTCCAUugAwIBAgIUX1qVhwS8EBcyQcSj/kipoocLGNAwCgYIKoZIzj0EAwIw
HzEdMBsGA1UEAwwUUHVtcGtpbiB0ZXN0IGVkZ2UgQ0EwIBcNMjYxMTI1MTcwODE4
WhgPMjEyNjExMDExNzA4MThaMB8xHTAbBgNVBAMMFFB1bXBraW4gdGVzdCBlZGdl
IENBMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEY164RmC4MPXW87raiMlSOVe/
g7HlrBnS3+zj1huexm7yyPzlly12JS+JeDLKZcyRdzSPfFmXsBMruVKJTGQfh6Nj
MGEwHQYDVR0OBBYEFAOBRr6eg0g7mnp242fjeaJ2iR6HMB8GA1UdIwQYMBaAFAOB
Rr6eg0g7mnp242fjeaJ2iR6HMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQD
AgIEMAoGCCqGSM49BAMCA0gAMEUCIHwH1IGe0DLNpA2Umzj4v6DMzCLA8WpMVpWv
is73Kr9FAiEAyKCr5UdL5tniJpnWIuRKxcvQXOCcUJSVhao718kugyU=
-----END CERTIFICATE-----
";

/// The stand-in's certificate, for [`NAME`], issued from its root, in PEM.
const CERTIFICATE: &str = "\
-----BEGIN CERTIFICATE-----
MIIBxDCCAWqgAwIBAgIUfp8aIQV48VBpnvt4LAoUMw0Y88wwCgYIKoZIzj0EAwIw
HzEdMBsGA1UEAwwUUHVtcGtpbiB0ZXN0IGVkZ2UgQ0EwIBcNMjYxMTI1MTcwODE4
WhgPMjEyNjExMDExNzA4MThaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDBZMBMGByqG
SM49AgEGCCqGSM49AwEHA0IABG8MzAcolZjlUATi6HCd6ciL/Dz8zNnJS0x0Ly27
cvEdEZxlguQ7MEUTrnPrNzg0XhRewh+1c2MyJsA+F/RUPaSjgYwwgYkwFAYDVR0R
BA0wC4IJbG9jYWxob3N0MAwGA1UdEwEB/wQCMAAwDgYDVR0PAQH/BAQDAgeAMBMG
A1UdJQQMMAoGCCsGAQUFBwMBMB0GA1UdDgQWBBRyfvbanv0zmTVil4luAtJpLGTR
VjAfBgNVHSMEGDAWgBQDgUa+noNIO5p6duNn43midokehzAKBggqhkjOPQQDAgNI
ADBFAiAwUMXKhIuGFe2wVAcvCswSRSNV+xKFf6wlnw3SdwqZHQIhAMyAuU089J6N
rI7hmyifu8g6rC4SBV2yZIjVr4p2Lpul
-----END CERTIFICATE-----
";

/// The private key of [`CERTIFICATE`], in PEM.
const KEY: &str = "\
-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgduDj3lMErdXjX0KQ
q4er0QMUxd6j0ukG1dEhzdZUtW+hRANCAARvDMwHKJWY5VAE4uhwnenIi/w8/MzZ
yUtMdC8tu3LxHRGcZYLkOzBFE65z6zc4NF4UXsIftXNjMibAPhf0VD2k
-----END PRIVATE KEY-----
";

/// A second certificate from the stand-in's root, for [`OTHER_NAME`], in PEM.
const OTHER_CERTIFICATE: &str = "\
-----BEGIN CERTIFICATE-----
MIIBxDCCAWqgAwIBAgIUfp8aIQV48VBpnvt4LAoUMw0Y880wCgYIKoZIzj0EAwIw
HzEdMBsGA1UEAwwUUHVtcGtpbiB0ZXN0IGVkZ2UgQ0EwIBcNMjYxMTI1MTcwODE5
WhgPMjEyNjExMDExNzA4MTlaMBQxEjAQBgNVBAMMCWNvbXBhbmlvbjBZMBMGByqG
SM49AgEGCCqGSM49AwEHA0IABIyEepkDO5hh/2SZJFddqW/SRjrDnZESSvU4U37t
Fswenmsj9YFFA93SpNOlAfa6MF+YaYC8QsOY8qmI6q2l4vyjgYwwgYkwFAYDVR0R
BA0wC4IJY29tcGFuaW9uMAwGA1UdEwEB/wQCMAAwDgYDVR0PAQH/BAQDAgeAMBMG
A1UdJQQMMAoGCCsGAQUFBwMBMB0GA1UdDgQWBBTZdZV4orA0yEJ/kthdIey5339Y
oDAfBgNVHSMEGDAWgBQDgUa+noNIO5p6duNn43midokehzAKBggqhkjOPQQDAgNI
ADBFAiEA48UkwiJL70JL/IS+nTjUWnB32O/o7Dum8acSUGmhqCUCIBIpCRji+yBC
x/RFv+cicgJy6gI2xSga0xCbh3CLx8jE
-----END CERTIFICATE-----
";

/// The private key of [`OTHER_CERTIFICATE`], in PEM.
const OTHER_KEY: &str = "\
-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgZnz4xPPIbM8uGqDE
n46XY08Ewklye6+D8U+oOiqYFXmhRANCAASMhHqZAzuYYf9kmSRXXalv0kY6w52R
Ekr1OFN+7RbMHp5rI/WBRQPd0qTTpQH2ujBfmGmAvELDmPKpiOqtpeL8
-----END PRIVATE KEY-----
";

/// A TLS server that shows the stand-in's certificate.
///
/// # Panics
///
/// Panics if the stand-in's certificates or key, constants, can't be read.
pub fn server() -> tls::Server {
    shows(CERTIFICATE, KEY)
}

/// A TLS server that the stand-in's root vouches for under [`OTHER_NAME`], as a public authority
/// may vouch for a server that is no sandbox's.
///
/// # Panics
///
/// Panics if its certificate or key, constants, can't be read.
pub fn other_server() -> tls::Server {
    shows(OTHER_CERTIFICATE, OTHER_KEY)
}

fn shows(certificate: &str, key: &str) -> tls::Server {
    let certificate = CertificateDer::from_pem_slice(certificate.as_bytes()).unwrap();
    let key = PrivatePkcs8KeyDer::from_pem_slice(key.as_bytes()).unwrap();
    tls::Server::new(&[&certificate], key.secret_pkcs8_der()).unwrap()
}

/// A TLS client that trusts the stand-in's root alone.
///
/// # Panics
///
/// Panics if the stand-in's root, a constant, can't be read.
pub fn client() -> tls::Client {
    let root = CertificateDer::from_pem_slice(ROOT.as_bytes()).unwrap();
    tls::Client::trusting(&root).unwrap()
}

/// A TLS client that trusts Web PKI's roots, as the driver's does, and so not the stand-in's.
pub fn web_pki() -> tls::Client {
    tls::Client::trusting_roots(webpki_roots::TLS_SERVER_ROOTS)
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

/// Listens in front of `serve`, which gets each connection that the stand-in carries on, one at
/// a time. Returns the port, as [`front`] does.
///
/// # Panics
///
/// Panics if it can't listen.
pub fn front_of(mut serve: impl FnMut(TcpStream) + Send + 'static) -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = front(listener.local_addr().unwrap());
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            serve(stream);
        }
    });
    port
}

/// Ends the TLS of `stream`, and copies it both ways with a connection to `target`.
fn relay(stream: TcpStream, target: SocketAddr) {
    let Ok(secured) = server().accept(stream, Deadline::after(HUNG)) else {
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
