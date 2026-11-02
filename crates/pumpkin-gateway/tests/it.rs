//! The gateway's end of a session, against a fake sandbox that answers the gateway's first
//! request with frames of its choosing, ops among them, waiting where it chooses for an op's
//! reply, and then ends the session: a sandbox running an agent's code can send anything.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]

use std::fs;
use std::net::Shutdown;
use std::num::NonZeroU64;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::time::Duration;

use expect_test::{Expect, expect};
use pumpkin_gateway::{Arrival, Listener, RefusedOp, Session};
use pumpkin_schema::ir::{AbsolutePath, Argv, Env, Resources};
use pumpkin_schema::sandbox::op::{OpReply, OpsReply};
use pumpkin_schema::sandbox::tree::Limits;
use pumpkin_schema::sandbox::{Launch, MAX_OP_PAYLOAD_BYTES, OpPayloadLimit, ToSandbox, frame};
use serde_json::{Value, json};

/// What the gateway asks of the fake sandbox, and what came of it.
type Action = fn(&mut Session, &Path) -> String;

/// The frames the fake sandbox sends in answer to the request on `stream`, with
/// [`await_reply`] where it waits, before it ends the session.
type Answer = fn(stream: u32) -> Vec<(Value, Vec<u8>)>;

/// How long the session's owner waits for an op or for the session's end before the test
/// fails: only a test that hangs waits this long, so no outcome rests on it.
const HUNG: Duration = Duration::from_secs(60);

/// Runs each case against a fake sandbox of its own, whose session takes ops within `ops`, if
/// it gives a limit, in a directory named after `name`, and checks what came of them against
/// `expect`.
fn check(name: &str, ops: Option<OpPayloadLimit>, cases: &[(Action, Answer)], expect: &Expect) {
    let id = std::process::id();
    let dir = std::env::temp_dir().join(format!("pumpkin-gateway-{id}-{name}"));
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    fs::create_dir_all(&dir).unwrap();
    let outcomes: Vec<String> = cases
        .iter()
        .enumerate()
        .map(|(n, (action, answer))| run(&dir.join(n.to_string()), ops, *action, *answer))
        .collect();
    expect.assert_eq(&outcomes.join("\n"));
    fs::remove_dir_all(dir).unwrap();
}

/// What came of `action`, and the op replies the fake sandbox heard, if any.
fn run(dir: &Path, ops: Option<OpPayloadLimit>, action: Action, answer: Answer) -> String {
    fs::create_dir(dir).unwrap();
    let socket = dir.join("gateway.sock");
    let listener = Listener::bind(socket.clone()).unwrap();
    let sandbox_socket = socket.clone();
    let sandbox = thread::spawn(move || {
        let mut connection = UnixStream::connect(sandbox_socket).unwrap();
        let (request, _) = frame::read::<ToSandbox>(&mut connection).unwrap().unwrap();
        let mut replies = Vec::new();
        for (message, body) in answer(stream_of(&request).unwrap()) {
            let went_on = if message.is_null() {
                hear_reply(&mut connection, &mut replies)
            } else {
                frame::write(&mut connection, &message, &body).is_ok()
            };
            // The gateway may hang up at any frame, leaving the rest unheard.
            if !went_on {
                break;
            }
        }
        // End the session, as a sandbox that closes its end of the connection does, but note
        // the op replies until the gateway closes its end too. A connection the gateway has
        // closed already is fine.
        let _ = connection.shutdown(Shutdown::Write);
        while hear_reply(&mut connection, &mut replies) {}
        replies
    });
    let mut session = listener.accept(Duration::from_secs(10), ops).unwrap();
    let second = UnixStream::connect(&socket).unwrap_err().kind();
    assert_eq!(second, std::io::ErrorKind::NotFound, "a second connection");
    let outcome = action(&mut session, dir);
    drop(session);
    let replies = sandbox.join().unwrap();
    if replies.is_empty() {
        outcome
    } else {
        format!("{outcome}; the sandbox heard {}", replies.join(", "))
    }
}

/// Reads what the gateway sends until an op's reply, which it notes in `replies`: `false` if
/// the session ends first.
fn hear_reply(connection: &mut UnixStream, replies: &mut Vec<String>) -> bool {
    while let Ok(Some((message, _))) = frame::read::<ToSandbox>(connection) {
        if let ToSandbox::OpReply { op, reply } = message {
            let reply = serde_json::to_string(&reply).unwrap();
            replies.push(format!("op {op}: {reply}"));
            return true;
        }
    }
    false
}

/// The stream a request names: `None` for an op's reply.
fn stream_of(request: &ToSandbox) -> Option<u32> {
    match *request {
        ToSandbox::Put { stream, .. }
        | ToSandbox::Entry { stream, .. }
        | ToSandbox::Data { stream }
        | ToSandbox::End { stream }
        | ToSandbox::Start { stream, .. }
        | ToSandbox::Freeze { stream }
        | ToSandbox::Collect { stream, .. } => Some(stream),
        ToSandbox::OpReply { .. } => None,
    }
}

/// Collects `/out` within small limits, and lists the files that arrived.
fn collect(session: &mut Session, dir: &Path) -> String {
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
fn start_and_freeze(session: &mut Session, _: &Path) -> String {
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

/// Asks for an op payload limit over the protocol's, which is refused, then answers ops as
/// [`ops`] does.
fn over_the_cap(session: &mut Session, dir: &Path) -> String {
    let refused = OpPayloadLimit::try_from(MAX_OP_PAYLOAD_BYTES + 1).unwrap_err();
    format!("op payload limit: {refused}; {}", ops(session, dir))
}

/// Freezes the sandbox, whose answer may carry ops, then answers each op that arrives whole,
/// and notes each the gateway refused, until the session ends.
fn ops(session: &mut Session, _: &Path) -> String {
    let mut lines = vec![format!("freeze: {}", outcome(session.freeze()))];
    loop {
        match session.next_op(HUNG) {
            Ok(Some(Arrival::Op(op))) => {
                lines.push(format!("{:?}", op.op));
                let reply = OpReply::Ops(OpsReply {
                    remaining_seconds: 0,
                    resources: Resources {
                        cpu_millis: NonZeroU64::MIN,
                        memory_bytes: NonZeroU64::MIN,
                    },
                    grants: Vec::new(),
                });
                if let Err(error) = session.reply(op, reply) {
                    lines.push(format!("reply: {:?}: {error}", error.kind()));
                }
            }
            Ok(Some(Arrival::Refused(refused))) => {
                let RefusedOp {
                    call,
                    size,
                    payload_limit,
                    message,
                    ..
                } = refused;
                lines.push(format!(
                    "refused {call:?} of {size} bytes over {}: {message}",
                    payload_limit.bytes()
                ));
            }
            Ok(None) => panic!("neither an op nor the session's end came within {HUNG:?}"),
            Err(error) => {
                lines.push(format!("{:?}: {error}", error.kind()));
                break;
            }
        }
    }
    lines.join("; ")
}

/// Freezes the sandbox, whose answer carries two ops, one after the other, then answers the
/// first with a reply too long for a frame, and the second with a short one.
fn too_long_reply(session: &mut Session, _: &Path) -> String {
    let mut lines = vec![format!("freeze: {}", outcome(session.freeze()))];
    for message in ["x".repeat(frame::MAX_MESSAGE_BYTES), "no".to_owned()] {
        lines.push(match session.next_op(HUNG).unwrap().unwrap() {
            Arrival::Op(op) => outcome(session.reply(op, OpReply::Refused { message })),
            Arrival::Refused(refused) => format!("{refused:?}"),
        });
    }
    lines.join("; ")
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

fn op(number: u32, call: Value, size: u64) -> (Value, Vec<u8>) {
    let mut message = json!({ "type": "op", "op": number, "size": size });
    message["call"] = call;
    (message, Vec::new())
}

fn train(number: u32, size: u64) -> (Value, Vec<u8>) {
    op(number, json!({ "kind": "judge", "grant": "train" }), size)
}

fn finalize(number: u32, size: u64) -> (Value, Vec<u8>) {
    let call = json!({ "kind": "submit", "grant": "submit", "finalize": true });
    op(number, call, size)
}

fn op_data(number: u32, bytes: &[u8]) -> (Value, Vec<u8>) {
    (json!({ "type": "op_data", "op": number }), bytes.to_vec())
}

/// Not a frame: where the fake sandbox waits for the reply to the op in flight, as `sandboxd`
/// does before it opens another.
fn await_reply() -> (Value, Vec<u8>) {
    (Value::Null, Vec::new())
}

fn done(stream: u32) -> (Value, Vec<u8>) {
    (json!({ "type": "done", "stream": stream }), Vec::new())
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
        "answers",
        None,
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
            // The command takes its start's `done` and its exit, so a `done` more fills its
            // queue, which holds two frames, and the next is one too many.
            (start_and_freeze, |s| vec![done(s); 4]),
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

/// Ops the sandbox makes, which the gateway passes on whole, refuses, or ends the session for,
/// in a session that takes ops whose payloads hold at most 4 bytes, and in one that takes none.
#[test]
fn untrusted_ops() {
    check(
        "ops",
        Some(OpPayloadLimit::try_from(4).unwrap()),
        &[
            // A payload over the limit is refused once all of its bytes have arrived, and the
            // next op follows; an op without a payload is whole at once.
            (ops, |s| {
                vec![
                    done(s),
                    train(7, 5),
                    op_data(7, b"abc"),
                    op_data(7, b"de"),
                    await_reply(),
                    finalize(8, 4),
                    op_data(8, b"ab"),
                    op_data(8, b"cd"),
                    await_reply(),
                    op(9, json!({ "kind": "status", "job": 3 }), 0),
                    await_reply(),
                ]
            }),
            (ops, |s| vec![done(s), op_data(7, b"x")]),
            (ops, |s| vec![done(s), train(7, MAX_OP_PAYLOAD_BYTES + 1)]),
            (ops, |s| {
                vec![done(s), op(9, json!({ "kind": "status", "job": 3 }), 3)]
            }),
            (ops, |s| vec![done(s), train(7, 2), op_data(7, b"abc")]),
            // An op over the limit is in flight from its `Op`, while its bytes arrive and before
            // its refusal, which goes only after its last byte: so opening another breaks the
            // protocol, and nothing is handed out.
            (ops, |s| {
                let ops = json!({ "kind": "ops" });
                vec![done(s), train(7, 5), op_data(7, b"abcd"), op(8, ops, 0)]
            }),
            // Data for an op whose payload has all arrived.
            (ops, |_| {
                vec![train(7, 1), op_data(7, b"a"), op_data(7, b"x")]
            }),
            // An op is in flight until answered, and the session ends before the freeze is
            // answered, with the op queued.
            (ops, |_| {
                vec![train(7, 0), op(8, json!({ "kind": "ops" }), 0)]
            }),
            // An op over the limit is handed out refused though the session ended before the
            // refusal could be sent.
            (ops, |_| {
                vec![train(7, 5), op_data(7, b"abcde"), op_data(99, b"x")]
            }),
            (too_long_reply, |s| {
                let ops = json!({ "kind": "ops" });
                vec![
                    done(s),
                    op(7, ops.clone(), 0),
                    await_reply(),
                    op(8, ops, 0),
                    await_reply(),
                ]
            }),
        ],
        &expect![[r#"
            freeze: ok; refused Judge { grant: Name("train") } of 5 bytes over 4: the payload is 5 bytes, more than the limit of 4: send at most 4; Submit { grant: Name("submit"), finalize: true, payload: [97, 98, 99, 100] }; Status { job: 3 }; ConnectionAborted: the sandbox ended the session; the sandbox heard op 7: {"kind":"refused","message":"the payload is 5 bytes, more than the limit of 4: send at most 4"}, op 8: {"kind":"ops","remaining_seconds":0,"resources":{"cpu_millis":1,"memory_bytes":1},"grants":[]}, op 9: {"kind":"ops","remaining_seconds":0,"resources":{"cpu_millis":1,"memory_bytes":1},"grants":[]}
            freeze: ok; ConnectionAborted: the sandbox broke the protocol: it sent data for op 7, which is not open
            freeze: ok; ConnectionAborted: the sandbox broke the protocol: it opened op 7 with a payload of 67108865 bytes, more than the protocol's 67108864
            freeze: ok; ConnectionAborted: the sandbox broke the protocol: it opened op 9 with a payload of 3 bytes, for a call that takes none
            freeze: ok; ConnectionAborted: the sandbox broke the protocol: it sent 3 bytes of op 7's payload, past its size: 2 were left
            freeze: ok; ConnectionAborted: the sandbox broke the protocol: it opened op 8 while op 7 is in flight
            freeze: ConnectionAborted: the sandbox broke the protocol: it sent data for op 7, which is not open; Judge { grant: Name("train"), payload: [97] }; reply: ConnectionAborted: the sandbox broke the protocol: it sent data for op 7, which is not open; ConnectionAborted: the sandbox broke the protocol: it sent data for op 7, which is not open
            freeze: ConnectionAborted: the sandbox broke the protocol: it opened op 8 while op 7 is in flight; Judge { grant: Name("train"), payload: [] }; reply: ConnectionAborted: the sandbox broke the protocol: it opened op 8 while op 7 is in flight; ConnectionAborted: the sandbox broke the protocol: it opened op 8 while op 7 is in flight
            freeze: ConnectionAborted: the sandbox broke the protocol: it sent data for op 99, which is not open; refused Judge { grant: Name("train") } of 5 bytes over 4: the payload is 5 bytes, more than the limit of 4: send at most 4; ConnectionAborted: the sandbox broke the protocol: it sent data for op 99, which is not open
            freeze: ok; InvalidInput: the message is 65602 bytes: a frame's message holds at most 65536; ok; the sandbox heard op 7: {"kind":"refused","message":"the reply was too long to send: the message is 65602 bytes: a frame's message holds at most 65536"}, op 8: {"kind":"refused","message":"no"}"#]],
    );
    check(
        "passive",
        None,
        &[(over_the_cap, |s| vec![done(s), train(7, 0)])],
        &expect![
            "op payload limit: the op payload limit is 67108865 bytes, over the protocol's 67108864: give at most that; freeze: ok; ConnectionAborted: the sandbox broke the protocol: it opened op 7, but the session takes no ops"
        ],
    );
}
