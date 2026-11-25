//! The links that the gateway carries between the sessions of a pod's containers, against fake
//! sandboxes that send link frames of their choosing: what reaches the other end, what is refused,
//! what a session's end leaves the other, and what breaks the protocol.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};

use expect_test::expect;
use gateway::{Arrival, Pod, Session, Sockets};
use schema::frame;
use schema::ir::{CompanionName, LinkPort, Member, Name};
use schema::sandbox::{
    FromSandbox, Hello, HelloReply, LINK_WINDOW_BYTES, SOCKET_NAME, SocketDial, ToSandbox,
};
use stdx::time::Machine;
use test_support::{HUNG, TestDir, hung};

/// A fake sandbox, met: its end of its session's connection, and the gateway's.
#[derive(Debug)]
struct Fake {
    connection: UnixStream,
    session: Session<Machine>,
    arrivals: Receiver<Arrival>,
}

fn member(name: &str) -> Member {
    match name {
        "main" => Member::Main,
        name => Member::Companion(CompanionName::try_from(name.to_owned()).unwrap()),
    }
}

fn port(port: u16) -> LinkPort {
    LinkPort::try_from(port).unwrap()
}

// A container of a pod by its name, with the links it may ask for, each a name and a port, if it
// carries any.
type Placed<'a> = (&'a str, Option<&'a [(&'a str, u16)]>);

// A frame that a container sends, by its name.
type Sent<'a> = (&'a str, FromSandbox, &'a [u8]);

/// Meets a fake sandbox for each of `members`, each in its place in one pod.
fn pod(dir: &Path, members: &[Placed<'_>]) -> Vec<Fake> {
    let sockets = Sockets::create(dir.join("sockets")).unwrap();
    let pod = Pod::default();
    let meet = |(name, reaches): &Placed<'_>| {
        let (SocketDial { dir }, registration) = sockets
            .issue(&Name::try_from((*name).to_owned()).unwrap())
            .unwrap();
        let mut connection = UnixStream::connect(dir.join(SOCKET_NAME)).unwrap();
        connection.set_read_timeout(Some(HUNG)).unwrap();
        frame::write(&mut connection, &Hello { gpus: None }, &[]).unwrap();
        let place = reaches.map(|reaches| {
            let reaches = reaches.iter().map(|(to, at)| (member(to), port(*at)));
            pod.place(member(name), reaches.collect::<BTreeSet<_>>())
        });
        let (sender, arrivals) = mpsc::channel();
        let accepted = registration.accept(
            (&Machine, hung()),
            None,
            (false, place),
            sender,
            |a| a,
            || false,
        );
        let session = accepted.unwrap();
        let reply = frame::read::<HelloReply>(&mut connection).unwrap();
        assert!(matches!(reply, Some((HelloReply::Accepted { .. }, _))));
        Fake {
            connection,
            session,
            arrivals,
        }
    };
    members.iter().map(meet).collect()
}

impl Fake {
    fn send(&mut self, message: &FromSandbox, body: &[u8]) {
        frame::write(&mut self.connection, message, body).unwrap();
    }

    /// The next frame that the gateway sends the sandbox, as a line.
    fn hear(&mut self) -> String {
        match frame::read::<ToSandbox>(&mut self.connection) {
            Ok(Some((message, body))) => note(message, &body),
            Ok(None) => "the gateway hung up".to_owned(),
            Err(error) => format!("{:?}: {error}", error.kind()),
        }
    }

    /// Why the session ended, which it does without the owner.
    fn ended(&self) -> String {
        match self.arrivals.recv_timeout(HUNG).unwrap() {
            Arrival::Ended { reason } => reason,
            other => panic!("the session's end was to arrive, not {other:?}"),
        }
    }
}

fn note(message: ToSandbox, body: &[u8]) -> String {
    match message {
        ToSandbox::LinkOpened { request, link } => {
            format!("link request {request}: opened link {link}")
        }
        ToSandbox::LinkRefused { request, message } => {
            format!("link request {request}: refused: {message}")
        }
        ToSandbox::LinkConnect { link, port } => format!("link {link}: connect to {port}"),
        ToSandbox::LinkData { link } if body.len() > 16 => {
            format!("link {link}: {} bytes", body.len())
        }
        ToSandbox::LinkData { link } => format!("link {link}: {:?}", String::from_utf8_lossy(body)),
        ToSandbox::LinkCredit { link, bytes } => format!("link {link}: credit {bytes}"),
        ToSandbox::LinkEnd { link } => format!("link {link}: end"),
        other => format!("{other:?}"),
    }
}

fn open(request: u32, to: &str, at: u16) -> FromSandbox {
    FromSandbox::LinkOpen {
        request,
        to: member(to),
        port: port(at),
    }
}

// What a link carries, both ways, and how it ends; what a pod refuses; and what a session's end
// leaves the other end of its links.
#[test]
fn carries() {
    let dir = TestDir::new();
    let reaches: &[(&str, u16)] = &[("scheduler", 8000), ("worker", 8000)];
    let fakes = pod(
        dir.path(),
        &[("main", Some(reaches)), ("scheduler", Some(&[]))],
    );
    let [mut main, mut scheduler] = <[Fake; 2]>::try_from(fakes).unwrap();
    let mut lines = Vec::new();
    let heard = |lines: &mut Vec<String>, name: &str, fake: &mut Fake| {
        lines.push(format!("{name} heard {}", fake.hear()));
    };
    main.send(&open(7, "scheduler", 8000), &[]);
    heard(&mut lines, "scheduler", &mut scheduler);
    heard(&mut lines, "main", &mut main);
    main.send(&FromSandbox::LinkData { link: 1 }, b"hello");
    heard(&mut lines, "scheduler", &mut scheduler);
    scheduler.send(&FromSandbox::LinkCredit { link: 1, bytes: 5 }, &[]);
    heard(&mut lines, "main", &mut main);
    scheduler.send(&FromSandbox::LinkData { link: 1 }, b"hi");
    heard(&mut lines, "main", &mut main);
    main.send(&FromSandbox::LinkEnd { link: 1 }, &[]);
    heard(&mut lines, "scheduler", &mut scheduler);
    scheduler.send(&FromSandbox::LinkEnd { link: 1 }, &[]);
    heard(&mut lines, "main", &mut main);
    // The grant of the last bytes, after the link closed, is no break.
    main.send(&FromSandbox::LinkCredit { link: 1, bytes: 2 }, &[]);
    main.send(&open(8, "scheduler", 9000), &[]);
    heard(&mut lines, "main", &mut main);
    main.send(&open(9, "worker", 8000), &[]);
    heard(&mut lines, "main", &mut main);
    main.send(&open(10, "scheduler", 8000), &[]);
    heard(&mut lines, "scheduler", &mut scheduler);
    heard(&mut lines, "main", &mut main);
    main.send(&FromSandbox::LinkData { link: 2 }, &[0; 100]);
    heard(&mut lines, "scheduler", &mut scheduler);
    let Fake {
        connection,
        session,
        arrivals: _,
    } = scheduler;
    drop((session, connection));
    lines.push("the scheduler's session ended".to_owned());
    heard(&mut lines, "main", &mut main);
    heard(&mut lines, "main", &mut main);
    main.send(&FromSandbox::LinkData { link: 2 }, b"late");
    heard(&mut lines, "main", &mut main);
    main.send(&FromSandbox::LinkEnd { link: 2 }, &[]);
    main.send(&open(11, "scheduler", 8000), &[]);
    heard(&mut lines, "main", &mut main);
    expect![[r#"
        scheduler heard link 1: connect to 8000
        main heard link request 7: opened link 1
        scheduler heard link 1: "hello"
        main heard link 1: credit 5
        main heard link 1: "hi"
        scheduler heard link 1: end
        main heard link 1: end
        main heard link request 8: refused: no link of the pod joins `main` to `scheduler` at port 9000
        main heard link request 9: refused: `worker`'s session isn't open: expected a link to a container while its session lasts
        scheduler heard link 2: connect to 8000
        main heard link request 10: opened link 2
        scheduler heard link 2: 100 bytes
        the scheduler's session ended
        main heard link 2: credit 100
        main heard link 2: end
        main heard link 2: credit 4
        main heard link request 11: refused: `scheduler`'s session isn't open: expected a link to a container while its session lasts"#]].assert_eq(&lines.join("\n"));
}

// What a sandbox sends of a link that the gateway ends its session for.
#[test]
fn protocol_breaks() {
    let dir = TestDir::new();
    let window = vec![0; usize::try_from(LINK_WINDOW_BYTES).unwrap() + 1];
    let cases: [(&str, &[Sent<'_>]); 6] = [
        (
            "past the window",
            &[("main", FromSandbox::LinkData { link: 1 }, &window)],
        ),
        (
            "on a link never opened",
            &[("main", FromSandbox::LinkData { link: 5 }, b"x")],
        ),
        (
            "a grant of more than was sent",
            &[(
                "scheduler",
                FromSandbox::LinkCredit { link: 1, bytes: 1 },
                &[],
            )],
        ),
        (
            "an end twice",
            &[
                ("main", FromSandbox::LinkEnd { link: 1 }, &[]),
                ("main", FromSandbox::LinkEnd { link: 1 }, &[]),
            ],
        ),
        (
            "bytes after the end",
            &[
                ("main", FromSandbox::LinkEnd { link: 1 }, &[]),
                ("main", FromSandbox::LinkData { link: 1 }, b"x"),
            ],
        ),
        (
            "a link asked for without a place",
            &[("worker", open(1, "main", 8000), &[])],
        ),
    ];
    let mut lines = Vec::new();
    for (n, (case, frames)) in cases.into_iter().enumerate() {
        let members = [
            ("main", Some([("scheduler", 8000)].as_slice())),
            ("scheduler", Some([].as_slice())),
            ("worker", None),
        ];
        let case_dir = dir.path().join(n.to_string());
        fs::create_dir(&case_dir).unwrap();
        let fakes = pod(&case_dir, &members);
        let [mut main, mut scheduler, worker] = <[Fake; 3]>::try_from(fakes).unwrap();
        main.send(&open(1, "scheduler", 8000), &[]);
        assert_eq!(scheduler.hear(), "link 1: connect to 8000");
        assert_eq!(main.hear(), "link request 1: opened link 1");
        let mut fakes = [("main", main), ("scheduler", scheduler), ("worker", worker)];
        let mut sender = "";
        for (name, message, body) in frames {
            sender = name;
            let (_, fake) = fakes.iter_mut().find(|(each, _)| each == name).unwrap();
            fake.send(message, body);
        }
        // Every session lasts until the sender's has ended, so that none leaves the pod first.
        let (_, fake) = fakes.iter().find(|(name, _)| *name == sender).unwrap();
        lines.push(format!("{case}: {}", fake.ended()));
    }
    expect![[r#"
        past the window: the sandbox broke the protocol: it sent 131073 bytes of link 1, past its window, of which 131072 bytes were left
        on a link never opened: the sandbox broke the protocol: it sent bytes of link 5, which is not open
        a grant of more than was sent: the sandbox broke the protocol: it granted back 1 bytes of link 1, more than the 0 it was sent and hadn't granted back
        an end twice: the sandbox broke the protocol: it ended link 1's way twice
        bytes after the end: the sandbox broke the protocol: it sent bytes of link 1 after it ended its way
        a link asked for without a place: the sandbox broke the protocol: it sent a frame of a link, but the session carries no links: send one only on a session accepted with a place in its pod"#]].assert_eq(&lines.join("\n"));
}
