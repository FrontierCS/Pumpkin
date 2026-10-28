//! The gateway's end of a session, against a fake sandbox that answers the gateway's first
//! request with frames of its choosing: a sandbox running an agent's code can send anything.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::fs;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::time::Duration;

use expect_test::{Expect, expect};
use pumpkin::gateway::{Listener, Session};
use pumpkin_schema::ir::{AbsolutePath, Argv, Env};
use pumpkin_schema::sandbox::tree::Limits;
use pumpkin_schema::sandbox::{Launch, ToSandbox, frame};
use serde_json::{Value, json};

/// What the gateway asks of the fake sandbox, and what came of it.
type Action = fn(&Session, &Path) -> String;

/// The frames the fake sandbox sends in answer to the request on `stream`.
type Answer = fn(stream: u32) -> Vec<(Value, Vec<u8>)>;

/// Runs each case against a fake sandbox of its own, and checks what came of them against
/// `expect`.
fn check(cases: &[(Action, Answer)], expect: &Expect) {
    let dir = std::env::temp_dir().join(format!("pumpkin-gateway-{}", std::process::id()));
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    fs::create_dir_all(&dir).unwrap();
    let outcomes: Vec<String> = cases
        .iter()
        .enumerate()
        .map(|(n, (action, answer))| run(&dir.join(n.to_string()), *action, *answer))
        .collect();
    expect.assert_eq(&outcomes.join("\n"));
    fs::remove_dir_all(dir).unwrap();
}

fn run(dir: &Path, action: Action, answer: Answer) -> String {
    fs::create_dir(dir).unwrap();
    let socket = dir.join("gateway.sock");
    let listener = Listener::bind(socket.clone()).unwrap();
    let sandbox_socket = socket.clone();
    let sandbox = thread::spawn(move || {
        let mut connection = UnixStream::connect(sandbox_socket).unwrap();
        let (request, _) = frame::read::<ToSandbox>(&mut connection).unwrap().unwrap();
        for (message, body) in answer(stream_of(&request)) {
            // The gateway may hang up at any frame, leaving the rest unheard.
            if frame::write(&mut connection, &message, &body).is_err() {
                break;
            }
        }
        // Answer nothing more, until the gateway ends the session.
        while let Ok(Some(_)) = frame::read::<ToSandbox>(&mut connection) {}
    });
    let session = listener.accept(Duration::from_secs(10)).unwrap();
    let second = UnixStream::connect(&socket).unwrap_err().kind();
    assert_eq!(second, std::io::ErrorKind::NotFound, "a second connection");
    let outcome = action(&session, dir);
    drop(session);
    sandbox.join().unwrap();
    outcome
}

fn stream_of(request: &ToSandbox) -> u32 {
    match *request {
        ToSandbox::Put { stream, .. }
        | ToSandbox::Entry { stream, .. }
        | ToSandbox::Data { stream }
        | ToSandbox::End { stream }
        | ToSandbox::Start { stream, .. }
        | ToSandbox::Freeze { stream }
        | ToSandbox::Collect { stream, .. } => stream,
    }
}

/// Collects `/out` within small limits, and lists the files that arrived.
fn collect(session: &Session, dir: &Path) -> String {
    let into = dir.join("collected");
    let limits = Limits {
        bytes: 8,
        entries: 3,
    };
    let out = AbsolutePath::try_from("/out".to_owned()).unwrap();
    let outcome = outcome(session.collect(&out, &into, limits));
    let mut arrived = walk(&into);
    arrived.sort();
    format!("{outcome}; arrived {arrived:?}")
}

fn outcome(outcome: std::io::Result<()>) -> String {
    match outcome {
        Ok(()) => "ok".to_owned(),
        Err(error) => format!("{:?}: {error}", error.kind()),
    }
}

/// Starts a command and, while it runs, freezes the sandbox.
fn start_and_freeze(session: &Session, _: &Path) -> String {
    let path = |path: &str| AbsolutePath::try_from(path.to_owned()).unwrap();
    let launch = Launch {
        argv: Argv::try_from(vec!["true".to_owned()]).unwrap(),
        env: Env::default(),
        cwd: path("/"),
        stdout: path("/stdout"),
        stderr: path("/stderr"),
    };
    let process = session.start(launch);
    let frozen = session.freeze();
    format!(
        "start: {}; freeze: {}",
        outcome(process.map(drop)),
        outcome(frozen)
    )
}

/// The files below `dir`, and the empty directories, each file with its contents.
fn walk(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().display().to_string();
        if path.is_dir() {
            let below = walk(&path);
            if below.is_empty() {
                found.push(format!("{name}/"));
            }
            found.extend(below.into_iter().map(|below| format!("{name}/{below}")));
        } else {
            found.push(format!("{name}={}", fs::read_to_string(&path).unwrap()));
        }
    }
    found
}

fn entry(stream: u32, entry: Value) -> (Value, Vec<u8>) {
    let mut message = json!({ "type": "entry", "stream": stream });
    message["entry"] = entry;
    (message, Vec::new())
}

fn dir(stream: u32, path: &str) -> (Value, Vec<u8>) {
    entry(stream, json!({ "kind": "dir", "path": path }))
}

fn file(stream: u32, path: &str, size: u64) -> (Value, Vec<u8>) {
    entry(
        stream,
        json!({ "kind": "file", "path": path, "size": size }),
    )
}

fn data(stream: u32, bytes: &[u8]) -> (Value, Vec<u8>) {
    (json!({ "type": "data", "stream": stream }), bytes.to_vec())
}

fn end(stream: u32) -> (Value, Vec<u8>) {
    (json!({ "type": "end", "stream": stream }), Vec::new())
}

fn reply(kind: &str, stream: u32) -> (Value, Vec<u8>) {
    let reply = match kind {
        "exited" => json!({ "type": kind, "stream": stream, "status": { "code": 0 } }),
        _ => json!({ "type": kind, "stream": stream, "message": "no" }),
    };
    (reply, Vec::new())
}

#[test]
fn untrusted_answers() {
    check(
        &[
            (collect, |s| {
                vec![dir(s, "a"), file(s, "a/b", 2), data(s, b"hi"), end(s)]
            }),
            (collect, |s| {
                vec![file(s, "../escape", 1), data(s, b"x"), end(s)]
            }),
            (collect, |s| vec![file(s, "/etc/x", 0)]),
            (collect, |s| {
                vec![entry(s, json!({ "kind": "link", "path": "a" }))]
            }),
            (collect, |s| vec![dir(s, "a/a/a")]),
            (collect, |s| vec![file(s, "a/b", 0)]),
            (collect, |s| vec![file(s, "big", 9)]),
            (collect, |s| {
                ["a", "b", "c", "d"].map(|path| dir(s, path)).to_vec()
            }),
            (collect, |s| vec![file(s, "f", 1), data(s, b"too long")]),
            (collect, |s| {
                vec![file(s, "f", 1), data(s, b""), data(s, b"x"), end(s)]
            }),
            (collect, |s| vec![file(s, "f", 2), end(s)]),
            (collect, |s| vec![reply("failed", s)]),
            (start_and_freeze, |s| {
                let mut flood = vec![(json!({ "type": "done", "stream": s }), Vec::new())];
                flood.extend((0..3).map(|_| reply("exited", s)));
                flood
            }),
        ],
        &expect![[r#"
            ok; arrived ["a/b=hi"]
            ConnectionAborted: the session failed: `../escape` is not a normalized relative path: use `/`-separated components, none of them empty, `.` or `..`, and no NUL byte; arrived []
            ConnectionAborted: the session failed: `/etc/x` is not a normalized relative path: use `/`-separated components, none of them empty, `.` or `..`, and no NUL byte; arrived []
            ConnectionAborted: the session failed: unknown variant `link`, expected `dir` or `file`; arrived []
            NotFound: a/a/a: No such file or directory (os error 2); arrived []
            NotFound: a/b: No such file or directory (os error 2); arrived []
            FileTooLarge: the tree's files hold more than 8 bytes; arrived []
            FileTooLarge: the tree has more than 3 entries; arrived ["a/", "b/", "c/"]
            InvalidData: file data runs past the file's size: send exactly the size given; arrived ["f="]
            InvalidData: file data is empty: send at least one byte in each frame; arrived ["f="]
            InvalidData: a file ended 2 bytes short: send all of its bytes before the next part; arrived ["f="]
            Other: no; arrived []
            start: ok; freeze: ConnectionAborted: the sandbox broke the protocol: it sent more on stream 1 than a request takes"#]],
    );
}
