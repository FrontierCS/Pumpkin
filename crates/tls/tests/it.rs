//! A TLS client's end and a server's, over the loopback, the server being the stand-in for a
//! provider's TLS edge (`test_support::edge`).

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::io::{Read as _, Write as _};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use expect_test::{Expect, expect};
use stdx::Deadline;
use test_support::{HUNG, edge};
use tls::{Client, Stream};

#[derive(Clone, Copy)]
enum Peer {
    Server,
    /// The stand-in's server, through a middle that, once told, acts on what the server sends
    /// next as this says.
    Through(Middle),
    /// A peer that sends these bytes, then nothing, and keeps the connection open.
    Sends(&'static [u8]),
}

#[derive(Clone, Copy)]
enum Middle {
    Cuts,
    Corrupts,
}

/// The two ends of a TCP connection over the loopback, the dialing one first.
fn connected() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let opened = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (accepted, _) = listener.accept().unwrap();
    (opened, accepted)
}

/// Copies `from` to `to` until `from` ends; once `middle`'s receiver has a message, acts as its
/// [`Middle`] says on the next bytes instead, and ends.
fn copy(mut from: TcpStream, mut to: TcpStream, middle: Option<&(Middle, mpsc::Receiver<()>)>) {
    let mut chunk = vec![0; 64 << 10];
    loop {
        let read = from.read(&mut chunk).unwrap_or(0);
        if read == 0 {
            let _ = to.shutdown(Shutdown::Write);
            return;
        }
        match middle {
            Some((Middle::Cuts, told)) if told.try_recv().is_ok() => {
                to.shutdown(Shutdown::Both).unwrap();
                return;
            }
            // An application-data record of TLS 1.3 that no key made.
            Some((Middle::Corrupts, told)) if told.try_recv().is_ok() => {
                to.write_all(b"\x17\x03\x03\x00\x13not the server's!!!")
                    .unwrap();
                return;
            }
            _ => to.write_all(&chunk[..read]).unwrap(),
        }
    }
}

/// Runs `client`'s end, for the server `name`, due within `within`, against `peer`, and compares
/// what `scenario` makes of the two ends' streams, given what tells the middle, if any, or each
/// end's outcome, with `expect`.
fn check(
    (client, name): (&Client, &str),
    peer: Peer,
    within: Duration,
    scenario: impl FnOnce(Stream, Stream, &dyn Fn()) -> Vec<String>,
    expect: &Expect,
) {
    let (client_end, server_end) = connected();
    let (client_end, server_end, tell) = match peer {
        Peer::Through(middle) => {
            let (to_middle, to_server) = connected();
            let (tell, told) = mpsc::channel();
            let (up, down) = (
                server_end.try_clone().unwrap(),
                to_middle.try_clone().unwrap(),
            );
            thread::spawn(move || copy(up, to_middle, None));
            thread::spawn(move || copy(down, server_end, Some(&(middle, told))));
            (client_end, to_server, Some(tell))
        }
        Peer::Server | Peer::Sends(_) => (client_end, server_end, None),
    };
    let shown = if let Peer::Sends(bytes) = peer {
        (&server_end).write_all(bytes).unwrap();
        let client = client.connect(client_end, name, Deadline::after(within));
        let client = client.map(drop).map_err(|e| e.to_string());
        vec![format!("client: {client:?}")]
    } else {
        let server =
            thread::spawn(move || edge::server().accept(server_end, Deadline::after(HUNG)));
        let client = client.connect(client_end, name, Deadline::after(within));
        let tell = || tell.as_ref().unwrap().send(()).unwrap();
        match (client, server.join().unwrap()) {
            (Ok(client), Ok(server)) => scenario(client, server, &tell),
            (client, server) => vec![
                format!("client: {:?}", client.map(drop).map_err(|e| e.to_string())),
                format!("server: {:?}", server.map(drop).map_err(|e| e.to_string())),
            ],
        }
    };
    expect.assert_eq(&shown.join("\n"));
}

// 8 MiB of a pattern: twice the 4 MiB under which `backpressure` expects a write to stall when
// the other end reads nothing.
fn pattern() -> Vec<u8> {
    (0..8u32 << 20).map(|i| (i % 251) as u8).collect()
}

/// Sends `bytes` from `from`, on a thread of its own, as `to` reads them on another: whether
/// they arrived intact.
fn sent(from: &Stream, to: &Stream, bytes: Vec<u8>) -> thread::JoinHandle<bool> {
    let (writer, reader) = (from.try_clone().unwrap(), to.try_clone().unwrap());
    thread::spawn(move || {
        let mut arrived = vec![0; bytes.len()];
        let writing = thread::spawn(move || (&writer).write_all(&bytes).map(|()| bytes));
        (&reader).read_exact(&mut arrived).unwrap();
        writing.join().unwrap().unwrap() == arrived
    })
}

#[test]
fn carries_both_ways() {
    check(
        (&edge::client(), edge::NAME),
        Peer::Server,
        HUNG,
        |client, server, _| {
            let (up, down) = (
                sent(&client, &server, pattern()),
                sent(&server, &client, pattern()),
            );
            let both = (up.join().unwrap(), down.join().unwrap());
            client.shutdown().unwrap();
            let ended = (&server).read_to_end(&mut Vec::new());
            let ended = ended.map_err(|error| error.to_string());
            vec![
                format!("both ways at once, intact: {both:?}"),
                format!("then the server's end reads: {ended:?}"),
            ]
        },
        &expect![[r#"
            both ways at once, intact: (true, true)
            then the server's end reads: Ok(0)"#]],
    );
}

#[test]
fn backpressure() {
    check(
        (&edge::client(), edge::NAME),
        Peer::Server,
        HUNG,
        |client, server, _| {
            server
                .set_write_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let bytes = pattern();
            let mut written = 0;
            let stalled = loop {
                match (&server).write(&bytes[written..]) {
                    Ok(wrote) => written += wrote,
                    Err(error) => break Some(error.kind()),
                }
                if written == bytes.len() {
                    break None;
                }
            };
            drop(client);
            vec![format!(
                "stalled: {stalled:?}, under 4 MiB: {}",
                written < 4 << 20
            )]
        },
        &expect!["stalled: Some(WouldBlock), under 4 MiB: true"],
    );
}

#[test]
fn broken_connections() {
    let broken = |client: Stream, server: Stream, tell: &dyn Fn()| {
        (&server).write_all(b"first").unwrap();
        let mut first = [0; 5];
        (&client).read_exact(&mut first).unwrap();
        tell();
        (&server).write_all(b"second").unwrap();
        let read = (&client).read_to_end(&mut Vec::new());
        let read = read.map_err(|error| (error.kind(), error.to_string()));
        let again = (&client).read(&mut [0; 8]).map_err(|error| error.kind());
        vec![
            format!("first: {}", String::from_utf8_lossy(&first)),
            format!("then: {read:?}"),
            format!("and again: {again:?}"),
        ]
    };
    let within = HUNG;
    for (middle, expect) in [
        (
            Middle::Cuts,
            expect![[r#"
            first: first
            then: Err((UnexpectedEof, "the peer's TCP connection ended without TLS's close_notify, as a connection cut short does: expected the peer to close it with one"))
            and again: Err(UnexpectedEof)"#]],
        ),
        (
            Middle::Corrupts,
            expect![[r#"
            first: first
            then: Err((InvalidData, "TLS failed: cannot decrypt peer's message"))
            and again: Err(InvalidData)"#]],
        ),
    ] {
        check(
            (&edge::client(), edge::NAME),
            Peer::Through(middle),
            within,
            broken,
            &expect,
        );
    }
}

fn unreached(_: Stream, _: Stream, _: &dyn Fn()) -> Vec<String> {
    unreachable!("the handshake was to fail")
}

#[test]
fn refused_servers() {
    let within = HUNG;
    check(
        (&edge::client(), "edge.example"),
        Peer::Server,
        within,
        unreached,
        &expect![[r#"
            client: Err("the TLS handshake failed: invalid peer certificate: certificate not valid for name \"edge.example\"; certificate is only valid for DnsName(\"localhost\"): expected TLS 1.3 from `edge.example`, with a certificate trusted for it")
            server: Err("the TLS handshake failed: received fatal alert: BadCertificate: expected a client's TLS 1.3")"#]],
    );
    check(
        (&Client::web_pki(), edge::NAME),
        Peer::Server,
        within,
        unreached,
        &expect![[r#"
            client: Err("the TLS handshake failed: invalid peer certificate: UnknownIssuer: expected TLS 1.3 from `localhost`, with a certificate trusted for it")
            server: Err("the TLS handshake failed: received fatal alert: UnknownCA: expected a client's TLS 1.3")"#]],
    );
    check(
        (&edge::client(), "127.0.0.1"),
        Peer::Sends(b""),
        within,
        unreached,
        &expect![[r#"
            client: Err("`127.0.0.1` is not a DNS name: give a server's")"#]],
    );
}

#[test]
fn refused_peers() {
    check(
        (&edge::client(), edge::NAME),
        Peer::Sends(b"{\"version\":1}\n"),
        HUNG,
        unreached,
        &expect![[
            r#"client: Err("the TLS handshake failed: received corrupt message of type InvalidContentType: expected TLS 1.3 from `localhost`, with a certificate trusted for it")"#
        ]],
    );
    check(
        (&edge::client(), edge::NAME),
        Peer::Sends(b""),
        Duration::from_millis(1),
        unreached,
        &expect![[
            r#"client: Err("the TLS handshake wasn't done by its deadline: expected TLS 1.3 from `localhost`, with a certificate trusted for it")"#
        ]],
    );
}

/// Runs `client`'s TLS for the server `name` within the TLS of the edge at `port`. Returns what
/// `scenario` says of its stream, or why it has none.
fn within(
    (client, name): (&Client, &str),
    port: u16,
    scenario: impl FnOnce(&Stream) -> String,
) -> String {
    let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let outer = edge::client().connect(tcp, edge::NAME, Deadline::after(HUNG));
    match client.connect_within(outer.unwrap(), name, Deadline::after(HUNG)) {
        Ok(stream) => scenario(&stream),
        Err(error) => format!("{:?}: {error}", error.kind()),
    }
}

/// Reads `expected`'s length from `stream`, and says what came, or why none did.
fn read(mut stream: &Stream, expected: &str) -> String {
    let mut bytes = vec![0; expected.len()];
    match stream.read_exact(&mut bytes) {
        Ok(()) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => format!("{:?}: {error}", error.kind()),
    }
}

#[test]
fn within_another_connection() {
    let (heard, server_heard) = mpsc::channel();
    // The server within answers, then says what the client sent before it closed.
    let answering = edge::front_of(move |stream| {
        let Ok(secured) = edge::other_server().accept(stream, Deadline::after(HUNG)) else {
            return;
        };
        (&secured).write_all(b"from within").unwrap();
        let mut sent = String::new();
        let read = (&secured).read_to_string(&mut sent).map(|_| sent);
        heard.send(read.map_err(|error| error.kind())).unwrap();
    });
    // It closes once the handshake's first bytes have come, so the client has sent them.
    let closing = edge::front_of(|mut stream| drop(stream.read(&mut [0])));
    // It cuts its TCP connection, below its TLS, once the client has what it sent.
    let cutting = edge::front_of(|stream| {
        let tcp = stream.try_clone().unwrap();
        let secured = edge::other_server().accept(stream, Deadline::after(HUNG));
        let secured = secured.unwrap();
        (&secured).write_all(b"first").unwrap();
        (&secured).read_exact(&mut [0]).unwrap();
        tcp.shutdown(Shutdown::Both).unwrap();
    });
    let named = (&edge::client(), edge::OTHER_NAME);
    let unopened = |_: &Stream| unreachable!("the handshake was to fail");
    let lines = [
        ("a server that the client trusts, by its name", {
            within(named, answering, |stream| {
                let read = read(stream, "from within");
                (&mut &*stream).write_all(b"to within").unwrap();
                stream.shutdown().unwrap();
                format!("{read}; the server read {:?}", server_heard.recv().unwrap())
            })
        }),
        ("one that it doesn't trust", {
            within((&Client::web_pki(), edge::OTHER_NAME), answering, unopened)
        }),
        ("by another name", {
            within((&edge::client(), "another"), answering, unopened)
        }),
        ("a far end that closes in the handshake", {
            within(named, closing, unopened)
        }),
        ("a far end cut short once the stream is open", {
            within(named, cutting, |stream| {
                let first = read(stream, "first");
                (&mut &*stream).write_all(b"k").unwrap();
                format!("{first}; then {}", read(stream, "more"))
            })
        }),
    ];
    let lines = lines.map(|(case, outcome)| format!("{case}: {outcome}"));
    expect![[r#"
        a server that the client trusts, by its name: from within; the server read Ok("to within")
        one that it doesn't trust: InvalidData: the TLS handshake failed: invalid peer certificate: UnknownIssuer: expected TLS 1.3 from `companion`, with a certificate trusted for it
        by another name: InvalidData: the TLS handshake failed: invalid peer certificate: certificate not valid for name "another"; certificate is only valid for DnsName("companion"): expected TLS 1.3 from `another`, with a certificate trusted for it
        a far end that closes in the handshake: UnexpectedEof: the TLS handshake failed: unexpected end of file: expected TLS 1.3 from `companion`, with a certificate trusted for it
        a far end cut short once the stream is open: first; then UnexpectedEof: the connection that carries this one ended without TLS's close_notify, as a connection cut short does: expected the peer to close it with one"#]].assert_eq(&lines.join("\n"));
}
