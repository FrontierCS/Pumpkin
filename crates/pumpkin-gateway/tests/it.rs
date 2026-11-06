//! The gateway's end of a session, against a fake sandbox that answers the gateway's first
//! request with frames of its choosing, ops among them, waiting where it chooses for an op's
//! reply, for a while or for the gateway to hang up, and then ends the session: it sends what
//! a buggy `sandboxd` might.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]

use std::fs;
use std::net::Shutdown;
use std::num::NonZeroU64;
use std::os::unix::net::UnixStream;
use std::panic;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use expect_test::{Expect, expect};
use pumpkin_gateway::{Arrival, IncomingOp, Listener, Process, Put, Session};
use pumpkin_schema::ir::{AbsolutePath, Argv, Env, Resources};
use pumpkin_schema::sandbox::op::{OpReply, OpsReply};
use pumpkin_schema::sandbox::tree::Limits;
use pumpkin_schema::sandbox::{
    Launch, MAX_OP_PAYLOAD_BYTES, OpPayloadLimit, RelativePath, ToSandbox, frame,
};
use serde_json::{Value, json};
use stdx::Deadline;

/// What arrives for the session's owner, with the instant the session's reader queued it.
type Arrived = (Arrival, Instant);

/// What the gateway asks of the fake sandbox, given what arrives, and what came of it.
type Action = fn(&mut Session, &Receiver<Arrived>, &Path) -> String;

/// The frames the fake sandbox sends in answer to the request on `stream`, with
/// [`await_reply`], [`pause`] and [`until_hang_up`] where it waits, before it ends the session.
type Answer = fn(stream: u32) -> Vec<(Value, Vec<u8>)>;

/// How long the session's owner waits for an op or for the session's end before the test
/// fails: only a test that hangs waits this long, so no outcome rests on it.
const HUNG: Duration = Duration::from_secs(60);

/// How long the fake sandbox reads and sends nothing where its answer has a [`pause`], as over
/// a slow link, whose end no event marks. A request by a [`hung`] deadline outlasts it, so no
/// outcome rests on a margin; a bound of the gateway's own shorter than it would show.
const PAUSE: Duration = Duration::from_millis(300);

/// A request's deadline, which only a test that hangs reaches.
fn hung() -> Deadline {
    Deadline::after(HUNG)
}

/// A request's deadline that passes soon, while the fake sandbox waits for the gateway to hang
/// up ([`until_hang_up`]).
fn short() -> Deadline {
    Deadline::after(Duration::from_millis(100))
}

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
///
/// # Panics
///
/// Panics unless `action` returns within [`HUNG`], which only an action that hangs takes.
fn run(dir: &Path, ops: Option<OpPayloadLimit>, action: Action, answer: Answer) -> String {
    let (mut session, arrivals, sandbox) = connect(dir, ops, answer);
    let dir = dir.to_owned();
    let (outcome, session, arrivals) = bounded("the action", move || {
        let outcome = action(&mut session, &arrivals, &dir);
        (outcome, session, arrivals)
    });
    heard(outcome, session, arrivals, sandbox)
}

/// What `f` returns, run on a thread of its own, so that an `f` that hangs fails the test
/// rather than hanging it.
///
/// # Panics
///
/// Panics if `f` does, or unless it returns within [`HUNG`], saying that `what` took longer.
fn bounded<R: Send + 'static>(what: &str, f: impl FnOnce() -> R + Send + 'static) -> R {
    let (returned, result) = mpsc::channel();
    let thread = thread::spawn(move || {
        // The receiver is gone only if the test has failed already.
        let _ = returned.send(f());
    });
    match result.recv_timeout(HUNG) {
        Ok(returned) => returned,
        Err(RecvTimeoutError::Timeout) => panic!("{what} took over {HUNG:?}"),
        Err(RecvTimeoutError::Disconnected) => panic::resume_unwind(thread.join().unwrap_err()),
    }
}

/// A session, accepted with `ops`, with a fake sandbox in the new directory `dir` that answers
/// the session's first request with `answer`'s frames, what arrives for the session's owner,
/// and the sandbox's thread, which returns the op replies it heard.
fn connect(
    dir: &Path,
    ops: Option<OpPayloadLimit>,
    answer: Answer,
) -> (Session, Receiver<Arrived>, JoinHandle<Vec<String>>) {
    fs::create_dir(dir).unwrap();
    let socket = dir.join("gateway.sock");
    let listener = Listener::bind(socket.clone()).unwrap();
    let sandbox_socket = socket.clone();
    let sandbox = thread::spawn(move || {
        let mut connection = UnixStream::connect(sandbox_socket).unwrap();
        // The gateway sends no request if it ends the session first.
        let Some((request, _)) = frame::read::<ToSandbox>(&mut connection).unwrap() else {
            return Vec::new();
        };
        let mut replies = Vec::new();
        for (message, body) in answer(stream_of(&request).unwrap()) {
            let went_on = if message.is_null() {
                hear_reply(&mut connection, &mut replies)
            } else if message == "pause" {
                thread::sleep(PAUSE);
                true
            } else if let Some(message) = message.get("until_hang_up") {
                await_hang_up(&mut connection, message, &body);
                false
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
    let (sender, arrivals) = mpsc::channel();
    let session = listener
        .accept(hung(), ops, sender, |arrival, at| (arrival, at))
        .unwrap();
    let second = UnixStream::connect(&socket).unwrap_err().kind();
    assert_eq!(second, std::io::ErrorKind::NotFound, "a second connection");
    (session, arrivals, sandbox)
}

/// `outcome`, and the op replies that `sandbox` heard by the end of `session`, if any.
///
/// # Panics
///
/// Panics unless dropping `session` returns within [`HUNG`], by when its reader has sent
/// whatever was left to arrive, and stopped.
fn heard(
    outcome: String,
    session: Session,
    arrivals: Receiver<Arrived>,
    sandbox: JoinHandle<Vec<String>>,
) -> String {
    let (left, closed) = bounded("dropping the session", move || {
        drop(session);
        let left = arrivals.try_iter().count();
        let closed = matches!(arrivals.try_recv(), Err(TryRecvError::Disconnected));
        (left, closed)
    });
    assert!(
        closed,
        "the session's reader ran on after its drop, with {left} arrivals sent"
    );
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

/// Sends the frame of `message` and `body` over and over, as fast as the gateway reads them,
/// and reads nothing, until the gateway hangs up, which a write then shows by failing.
///
/// # Panics
///
/// Panics if the gateway is still there after [`HUNG`].
fn await_hang_up(connection: &mut UnixStream, message: &Value, body: &[u8]) {
    let hung = hung();
    while frame::write(connection, message, body).is_ok() {
        assert!(
            hung.remaining().is_some(),
            "the gateway was still there after {HUNG:?}"
        );
    }
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
fn collect(session: &mut Session, _: &Receiver<Arrived>, dir: &Path) -> String {
    let into = dir.join("collected");
    let limits = Limits {
        bytes: 8,
        entries: 3,
    };
    let out = AbsolutePath::try_from("/out".to_owned()).unwrap();
    let outcome = outcome(session.collect(&out, &into, limits, hung()));
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

/// How to run `true`.
fn launch() -> Launch {
    let path = |path: &str| AbsolutePath::try_from(path.to_owned()).unwrap();
    Launch {
        argv: Argv::try_from(vec!["true".to_owned()]).unwrap(),
        env: Env::default(),
        cwd: path("/"),
        stdout: path("/stdout"),
        stderr: path("/stderr"),
    }
}

/// Starts a command and, while it runs, freezes the sandbox.
fn start_and_freeze(session: &mut Session, _: &Receiver<Arrived>, _: &Path) -> String {
    let process = session.start(launch(), hung());
    let frozen = session.freeze(hung());
    format!(
        "start: {}; freeze: {}",
        outcome(process.map(drop)),
        outcome(frozen)
    )
}

/// Asks for an op payload limit over the protocol's, which is refused, then answers ops as
/// [`ops`] does.
fn over_the_cap(session: &mut Session, arrivals: &Receiver<Arrived>, dir: &Path) -> String {
    let refused = OpPayloadLimit::try_from(MAX_OP_PAYLOAD_BYTES + 1).unwrap_err();
    format!(
        "op payload limit: {refused}; {}",
        ops(session, arrivals, dir)
    )
}

/// Freezes the sandbox, whose answer may carry ops, then takes what arrives ([`take`]).
fn ops(session: &mut Session, arrivals: &Receiver<Arrived>, _: &Path) -> String {
    let frozen = outcome(session.freeze(hung()));
    format!("freeze: {frozen}; {}", take(session, arrivals, None))
}

/// Starts a command, whose stream carries what the fake sandbox answers, then takes what
/// arrives ([`take`]).
fn start_and_take(session: &mut Session, arrivals: &Receiver<Arrived>, _: &Path) -> String {
    let process = session.start(launch(), hung()).unwrap();
    format!("started; {}", take(session, arrivals, Some(process)))
}

/// Answers each op that arrives, refuses each whose payload was over the limit, and notes each
/// command's exit, saying whether it is `started`'s, until the session's end, and then anything
/// that arrives after it.
fn take(session: &mut Session, arrivals: &Receiver<Arrived>, started: Option<Process>) -> String {
    let mut lines = Vec::new();
    loop {
        let Some((arrival, _)) = next(arrivals) else {
            panic!("the session's reader stopped before the session's end arrived");
        };
        match arrival {
            Arrival::Op(op) => {
                lines.push(format!("{:?}", op.op));
                if let Err(error) = session.reply(op, no_grants(), hung()) {
                    lines.push(format!("reply: {:?}: {error}", error.kind()));
                }
            }
            Arrival::Refused(refused) => {
                lines.push(format!(
                    "refused {:?} of {} bytes over {}: {}",
                    refused.call(),
                    refused.size(),
                    refused.payload_limit().bytes(),
                    refused.message()
                ));
                if let Err(error) = session.refuse(refused, hung()) {
                    lines.push(format!("refuse: {:?}: {error}", error.kind()));
                }
            }
            Arrival::Exited { process, status } => {
                let whose = if started == Some(process) {
                    "the"
                } else {
                    "another"
                };
                lines.push(format!("{whose} command exited: {status:?}"));
            }
            Arrival::Ended { reason } => {
                lines.push(format!("ended: {reason}"));
                break;
            }
        }
    }
    if let Some((after, _)) = next(arrivals) {
        lines.push(format!("then {after:?}"));
    }
    lines.join("; ")
}

/// The next arrival, or `None` once the session's reader has stopped.
///
/// # Panics
///
/// Panics if nothing arrives within [`HUNG`].
fn next(arrivals: &Receiver<Arrived>) -> Option<Arrived> {
    match arrivals.recv_timeout(HUNG) {
        Ok(arrived) => Some(arrived),
        Err(RecvTimeoutError::Disconnected) => None,
        Err(RecvTimeoutError::Timeout) => panic!("nothing arrived within {HUNG:?}"),
    }
}

/// The next arrival, which must be an op that arrived whole.
fn next_op(arrivals: &Receiver<Arrived>) -> (IncomingOp, Instant) {
    match next(arrivals) {
        Some((Arrival::Op(op), at)) => (op, at),
        arrived => panic!("an op was to arrive, not {arrived:?}"),
    }
}

/// Freezes the sandbox, whose answer carries two ops, one after the other, and answers each,
/// noting whether op 8, which the sandbox sends once op 7 is answered, was stamped after op 7
/// was taken, as it is if the stamp is when it arrived.
fn stamps(session: &mut Session, arrivals: &Receiver<Arrived>, _: &Path) -> String {
    session.freeze(hung()).unwrap();
    let (op, _) = next_op(arrivals);
    let taken = Instant::now();
    session.reply(op, no_grants(), hung()).unwrap();
    let (op, stamped) = next_op(arrivals);
    session.reply(op, no_grants(), hung()).unwrap();
    format!("op 8 stamped after op 7 was taken: {}", stamped >= taken)
}

/// An `ops` reply with no grants.
fn no_grants() -> OpReply {
    OpReply::Ops(OpsReply {
        remaining_seconds: 0,
        resources: Resources {
            cpu_millis: NonZeroU64::MIN,
            memory_bytes: NonZeroU64::MIN,
        },
        grants: Vec::new(),
    })
}

/// Starts a command on the session, whose sandbox's answer carries op 0, and on another
/// session, whose sandbox's answer carries op 0 and then op 1, over the limit, and notes
/// whether the two commands, each on its session's first stream, compare equal. Then answers
/// the first session's op 0 on the other session, the other's own, and refuses the other's op 1
/// on the first session.
fn answer_elsewhere(session: &mut Session, arrivals: &Receiver<Arrived>, dir: &Path) -> String {
    let ops = Some(OpPayloadLimit::try_from(0).unwrap());
    let (mut other, others, sandbox) = connect(&dir.join("other"), ops, |s| {
        vec![
            done(s),
            op(0, json!({ "kind": "ops" }), 0),
            await_reply(),
            train(1, 1),
            op_data(1, b"x"),
            await_reply(),
        ]
    });
    let [(mine, my_command), (own, its_command)] =
        [(&mut *session, arrivals), (&mut other, &others)].map(|(session, arrivals)| {
            let process = session.start(launch(), hung()).unwrap();
            (next_op(arrivals).0, process)
        });
    let message = "from another session".to_owned();
    let elsewhere = panicked(|| other.reply(mine, OpReply::Refused { message }, hung()));
    let own = outcome(other.reply(own, no_grants(), hung()));
    let refused = match next(&others) {
        Some((Arrival::Refused(refused), _)) => refused,
        arrived => panic!("a refused op was to arrive, not {arrived:?}"),
    };
    let refusal = panicked(|| session.refuse(refused, hung()));
    let outcome = format!(
        "the sessions' commands equal: {}; op 0 on another session: {elsewhere}; that session's \
         own op 0: {own}; its op 1 refused on another session: {refusal}",
        my_command == its_command
    );
    heard(outcome, other, others, sandbox)
}

/// What came of `answer`, or the message of its panic, on a thread of its own, where a panic
/// ends that thread alone, so the test goes on.
fn panicked(answer: impl FnOnce() -> std::io::Result<()> + Send) -> String {
    match thread::scope(|scope| scope.spawn(answer).join()) {
        Ok(answered) => outcome(answered),
        Err(panic) => format!("panicked: {}", panic.downcast::<String>().unwrap()),
    }
}

/// Freezes the sandbox, whose answer carries two ops, one after the other, then answers the
/// first with a reply too long for a frame, and the second with a short one.
fn too_long_reply(session: &mut Session, arrivals: &Receiver<Arrived>, _: &Path) -> String {
    let mut lines = vec![format!("freeze: {}", outcome(session.freeze(hung())))];
    for message in ["x".repeat(frame::MAX_MESSAGE_BYTES), "no".to_owned()] {
        let (op, _) = next_op(arrivals);
        lines.push(outcome(session.reply(
            op,
            OpReply::Refused { message },
            hung(),
        )));
    }
    lines.join("; ")
}

/// What came of `first`, a request, and then of freezing the sandbox by a deadline only a hang
/// reaches, as the request after a failed one does.
fn then_freeze(session: &mut Session, first: std::io::Result<()>) -> String {
    format!(
        "{}; then {}",
        outcome(first),
        outcome(session.freeze(hung()))
    )
}

/// Freezes the sandbox by `by`, then again ([`then_freeze`]).
fn freezes(session: &mut Session, by: Deadline) -> String {
    let first = session.freeze(by);
    then_freeze(session, first)
}

/// Freezes the sandbox, which then ends the session, and again ([`freezes`]), which fails only
/// once the session has ended, then once more by a deadline that has passed.
fn after_the_end(session: &mut Session, _: &Receiver<Arrived>, _: &Path) -> String {
    let ended = freezes(session, hung());
    let late = outcome(session.freeze(Deadline::after(Duration::ZERO)));
    format!("{ended}; then by a passed deadline {late}")
}

/// Puts a file larger than the connection's buffers hold, by `by`.
fn put_large(session: &mut Session, by: Deadline) -> std::io::Result<()> {
    let dir = AbsolutePath::try_from("/in".to_owned()).unwrap();
    let file = RelativePath::try_from("big".to_owned()).unwrap();
    let bytes = vec![0; 8 << 20];
    session.put(&dir, &[Put::File(&file, &bytes)], by)
}

/// Freezes the sandbox, whose answer carries an op, answers the op by a deadline that has
/// passed, and freezes it again ([`then_freeze`]).
fn late_reply(session: &mut Session, arrivals: &Receiver<Arrived>, _: &Path) -> String {
    session.freeze(hung()).unwrap();
    let (op, _) = next_op(arrivals);
    let replied = session.reply(op, no_grants(), Deadline::after(Duration::ZERO));
    then_freeze(session, replied)
}

/// Collects `/out` by a [`short`] deadline, within limits no tree reaches.
fn collect_short(session: &mut Session, _: &Receiver<Arrived>, dir: &Path) -> String {
    let limits = Limits {
        bytes: u64::MAX,
        entries: u64::MAX,
    };
    let out = AbsolutePath::try_from("/out".to_owned()).unwrap();
    outcome(session.collect(&out, &dir.join("collected"), limits, short()))
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

/// Not a frame: where the fake sandbox reads and sends nothing for [`PAUSE`], as a sandbox
/// over a slow link does.
fn pause() -> (Value, Vec<u8>) {
    (Value::from("pause"), Vec::new())
}

/// Not a frame: where the fake sandbox sends the frame it wraps over and over, reading nothing,
/// until the gateway hangs up ([`await_hang_up`]).
fn until_hang_up((message, body): (Value, Vec<u8>)) -> (Value, Vec<u8>) {
    (json!({ "until_hang_up": message }), body)
}

/// Where the fake sandbox reads and answers nothing until the gateway hangs up, as a stuck
/// sandbox does, sending only replies on stream 0, which no request uses, so the gateway drops
/// them.
fn stall() -> (Value, Vec<u8>) {
    until_hang_up(done(0))
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
fn sandbox_answers() {
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
            // A started command's stream carries only its exit, so a second `done` breaks the
            // protocol.
            (start_and_freeze, |s| vec![done(s); 2]),
            // The command's route goes at its exit, its stream's last frame, so later ones drop.
            (start_and_freeze, |s| {
                let mut flood = vec![done(s)];
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
            start: ok; freeze: ConnectionAborted: the sandbox broke the protocol: it sent more on stream 1 than a request takes
            start: ok; freeze: ConnectionAborted: the sandbox ended the session"#]],
    );
}

/// Ops the sandbox makes, which the gateway passes on whole, refuses, or ends the session for,
/// in a session that takes ops whose payloads hold at most 4 bytes, and in one that takes none.
#[test]
fn sandbox_ops() {
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
            freeze: ok; refused Judge { grant: Name("train") } of 5 bytes over 4: the payload is 5 bytes, more than the limit of 4: send at most 4; Submit { grant: Name("submit"), finalize: true, payload: [97, 98, 99, 100] }; Status { job: 3 }; ended: the sandbox ended the session; the sandbox heard op 7: {"kind":"refused","message":"the payload is 5 bytes, more than the limit of 4: send at most 4"}, op 8: {"kind":"ops","remaining_seconds":0,"resources":{"cpu_millis":1,"memory_bytes":1},"grants":[]}, op 9: {"kind":"ops","remaining_seconds":0,"resources":{"cpu_millis":1,"memory_bytes":1},"grants":[]}
            freeze: ok; ended: the sandbox broke the protocol: it sent data for op 7, which is not open
            freeze: ok; ended: the sandbox broke the protocol: it opened op 7 with a payload of 67108865 bytes, more than the protocol's 67108864
            freeze: ok; ended: the sandbox broke the protocol: it opened op 9 with a payload of 3 bytes, for a call that takes none
            freeze: ok; ended: the sandbox broke the protocol: it sent 3 bytes of op 7's payload, past its size: 2 were left
            freeze: ok; ended: the sandbox broke the protocol: it opened op 8 while op 7 is in flight
            freeze: ConnectionAborted: the sandbox broke the protocol: it sent data for op 7, which is not open; Judge { grant: Name("train"), payload: [97] }; reply: ConnectionAborted: the sandbox broke the protocol: it sent data for op 7, which is not open; ended: the sandbox broke the protocol: it sent data for op 7, which is not open
            freeze: ConnectionAborted: the sandbox broke the protocol: it opened op 8 while op 7 is in flight; Judge { grant: Name("train"), payload: [] }; reply: ConnectionAborted: the sandbox broke the protocol: it opened op 8 while op 7 is in flight; ended: the sandbox broke the protocol: it opened op 8 while op 7 is in flight
            freeze: ConnectionAborted: the sandbox broke the protocol: it sent data for op 99, which is not open; refused Judge { grant: Name("train") } of 5 bytes over 4: the payload is 5 bytes, more than the limit of 4: send at most 4; refuse: ConnectionAborted: the sandbox broke the protocol: it sent data for op 99, which is not open; ended: the sandbox broke the protocol: it sent data for op 99, which is not open
            freeze: ok; InvalidInput: the message is 65602 bytes: a frame's message holds at most 65536; ok; the sandbox heard op 7: {"kind":"refused","message":"the reply was too long to send: the message is 65602 bytes: a frame's message holds at most 65536"}, op 8: {"kind":"refused","message":"no"}"#]],
    );
    check(
        "passive",
        None,
        &[(over_the_cap, |s| vec![done(s), train(7, 0)])],
        &expect![
            "op payload limit: the op payload limit is 67108865 bytes, over the protocol's 67108864: give at most that; freeze: ok; ended: the sandbox broke the protocol: it opened op 7, but the session takes no ops"
        ],
    );
}

/// An op answered or refused on a session other than the one it arrived on panics before
/// anything is sent: every sandbox numbers its ops from 0, so the other's would take the answer
/// for one of its own. Two sessions' commands never compare equal, though each session numbers
/// its streams alike.
#[test]
fn another_sessions_op() {
    check(
        "elsewhere",
        Some(OpPayloadLimit::try_from(0).unwrap()),
        &[(answer_elsewhere, |s| {
            vec![done(s), op(0, json!({ "kind": "ops" }), 0), await_reply()]
        })],
        &expect![[
            r#"the sessions' commands equal: false; op 0 on another session: panicked: internal error: entered unreachable code: op 0 was answered on a session other than the one it arrived on; that session's own op 0: ok; its op 1 refused on another session: panicked: internal error: entered unreachable code: op 1 was answered on a session other than the one it arrived on; the sandbox heard op 0: {"kind":"ops","remaining_seconds":0,"resources":{"cpu_millis":1,"memory_bytes":1},"grants":[]}"#
        ]],
    );
}

/// Requests wait on the sandbox until their deadlines, and one whose deadline passes fails and
/// ends the session, however much of it went through; a sandbox that closes its connection
/// fails a request at once.
#[test]
fn deadlines() {
    check(
        "deadlines",
        Some(OpPayloadLimit::try_from(0).unwrap()),
        &[
            // The sandbox answers after a pause, or stops reading a large put for one, and then
            // reads on: the requests wait, as their deadlines haven't passed. It answers the put
            // before it has read it all, which the gateway takes once the put is all written.
            (
                |session, _, _| freezes(session, hung()),
                |s| vec![pause(), done(s)],
            ),
            (
                |session, _, _| outcome(put_large(session, hung())),
                |s| vec![pause(), done(s), await_reply()],
            ),
            // The sandbox answers nothing, reads nothing past a large put's first frame, or
            // sends a tree that never ends, until the gateway hangs up.
            (|session, _, _| freezes(session, short()), |_| vec![stall()]),
            (
                |session, _, _| {
                    let put = put_large(session, short());
                    then_freeze(session, put)
                },
                |_| vec![stall()],
            ),
            (collect_short, |s| {
                vec![file(s, "f", u64::MAX), until_hang_up(data(s, b"x"))]
            }),
            // In frames of a full body, which the request takes longer to write than the reader
            // takes to read, so that the reader waits on the request's queue as the deadline
            // passes, and must not go on waiting there once the request has failed.
            (collect_short, |s| {
                let body = vec![b'x'; frame::MAX_BODY_BYTES];
                vec![file(s, "f", u64::MAX), until_hang_up(data(s, &body))]
            }),
            // A deadline that has passed before a request or a reply starts, which sends nothing.
            (
                |session, _, _| freezes(session, Deadline::after(Duration::ZERO)),
                |_| Vec::new(),
            ),
            (late_reply, |s| {
                vec![done(s), op(0, json!({ "kind": "ops" }), 0), await_reply()]
            }),
            // A session that has ended gives its own reason, though the deadline has passed too.
            (after_the_end, |s| vec![done(s)]),
            (collect, |s| vec![dir(s, "a")]),
        ],
        &expect![[r#"
            ok; then ConnectionAborted: the sandbox ended the session
            ok
            TimedOut: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts; then ConnectionAborted: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts
            TimedOut: the sandbox didn't take what was sent before the request's deadline: expected it to read while the deadline lasts; then ConnectionAborted: the sandbox didn't take what was sent before the request's deadline: expected it to read while the deadline lasts
            TimedOut: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts
            TimedOut: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts
            TimedOut: the request's deadline passed before it was sent: expected time left to send it; then ConnectionAborted: the request's deadline passed before it was sent: expected time left to send it
            TimedOut: the request's deadline passed before it was sent: expected time left to send it; then ConnectionAborted: the request's deadline passed before it was sent: expected time left to send it
            ok; then ConnectionAborted: the sandbox ended the session; then by a passed deadline ConnectionAborted: the sandbox ended the session
            ConnectionAborted: the sandbox ended the session; arrived ["a/"]"#]],
    );
}

/// What arrives for the session's owner does in the order the sandbox sent it, stamped as the
/// session's reader queued it, and ending with the session's end, which a session that the
/// sandbox keeps open reaches as it drops ([`heard`]).
#[test]
fn arrivals() {
    check(
        "arrivals",
        Some(OpPayloadLimit::try_from(0).unwrap()),
        &[
            (stamps, |s| {
                let ops = json!({ "kind": "ops" });
                vec![
                    done(s),
                    op(7, ops.clone(), 0),
                    await_reply(),
                    op(8, ops, 0),
                    await_reply(),
                ]
            }),
            // An op the command made before it exited, its exit, and an op one of its children
            // made after.
            (start_and_take, |s| {
                vec![
                    done(s),
                    op(7, json!({ "kind": "ops" }), 0),
                    reply("exited", s),
                    await_reply(),
                    op(8, json!({ "kind": "status", "job": 3 }), 0),
                    await_reply(),
                ]
            }),
            // The sandbox goes on sending, reading nothing, until the gateway hangs up.
            (
                |session, _, _| outcome(session.freeze(hung())),
                |s| vec![done(s), stall()],
            ),
        ],
        &expect![[r#"
            op 8 stamped after op 7 was taken: true; the sandbox heard op 7: {"kind":"ops","remaining_seconds":0,"resources":{"cpu_millis":1,"memory_bytes":1},"grants":[]}, op 8: {"kind":"ops","remaining_seconds":0,"resources":{"cpu_millis":1,"memory_bytes":1},"grants":[]}
            started; Ops; the command exited: Code(0); Status { job: 3 }; ended: the sandbox ended the session; the sandbox heard op 7: {"kind":"ops","remaining_seconds":0,"resources":{"cpu_millis":1,"memory_bytes":1},"grants":[]}, op 8: {"kind":"ops","remaining_seconds":0,"resources":{"cpu_millis":1,"memory_bytes":1},"grants":[]}
            ok"#]],
    );
}
