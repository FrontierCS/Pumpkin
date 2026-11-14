//! The gateway's end of a session, against a fake sandbox that answers the gateway's first
//! request with frames of its choosing, ops and model requests among them, waiting where it
//! chooses for an op's reply or a model response's end, for a while or for the gateway to hang
//! up, and then ends the session: it sends what a buggy `sandboxd` might. Other fakes open their
//! sessions with first frames of their own. An accept that is stopped takes no connection.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]

use std::io::{Read as _, Write};
use std::net::Shutdown;
use std::num::NonZeroU64;
use std::ops::Range;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::{fs, panic};

use expect_test::{Expect, expect};
use gateway::{
    Arrival, IncomingModelRequest, IncomingOp, Process, Put, Registration, Session, Sockets,
};
use schema::ir::{AbsolutePath, Argv, Env, Name, Resources};
use schema::sandbox::op::{OpReply, OpsReply};
use schema::sandbox::tree::Limits;
use schema::sandbox::{
    Hello, HelloReply, Launch, MAX_HELLO_BYTES, MAX_MODEL_REQUEST_BYTES,
    MAX_MODEL_REQUESTS_IN_FLIGHT, MAX_OP_PAYLOAD_BYTES, OpPayloadLimit, PROTOCOL_VERSION,
    RegistrationGrant, RelativePath, SOCKET_NAME, SocketDial, ToSandbox, file_mode, frame,
};
use serde_json::{Value, json};
use stdx::Deadline;
use test_support::{HUNG, hung};

/// What the gateway asks of the fake sandbox, given what arrives, and what came of it.
type Action = fn(&mut Session, &Receiver<Arrival>, &Path) -> String;

/// The frames the fake sandbox sends in answer to the request on `stream`, with
/// [`await_reply`], [`pause`], [`until_hang_up`] and [`stall`] where it waits, before it ends the
/// session.
type Answer = fn(stream: u32) -> Vec<(Value, Vec<u8>)>;

/// What a fake sandbox sends first on its connection, if anything ([`opened`]).
type Opening = fn(&mut UnixStream);

/// How long the fake sandbox reads and sends nothing where its answer has a [`pause`], as over
/// a slow link, whose end no event marks. A request by a [`hung`] deadline outlasts it, so no
/// outcome rests on a margin; a bound of the gateway's own shorter than it would show.
const PAUSE: Duration = Duration::from_millis(300);

/// A request's deadline that passes soon, while the fake sandbox waits for the gateway to hang
/// up ([`until_hang_up`]).
fn short() -> Deadline {
    Deadline::after(Duration::from_millis(100))
}

/// Runs `scenario` in a directory of its own, and checks the lines it reports against `expect`.
fn check(scenario: impl FnOnce(&Path) -> Vec<String>, expect: &Expect) {
    // Kept if the test fails.
    let dir = tempfile::Builder::new()
        .prefix("gateway-")
        .tempdir()
        .unwrap()
        .keep();
    let lines = scenario(&dir);
    expect.assert_eq(&lines.join("\n"));
    fs::remove_dir_all(dir).unwrap();
}

/// A scenario that runs each case against a fake sandbox of its own, whose session takes ops
/// within `ops`, if it gives a limit, and model requests if `models`, in a directory of its own,
/// and reports what came of each.
fn fakes(
    ops: Option<OpPayloadLimit>,
    models: bool,
    cases: &[(Action, Answer)],
) -> impl FnOnce(&Path) -> Vec<String> {
    move |dir| {
        (cases.iter().enumerate())
            .map(|(n, (action, answer))| {
                run(&dir.join(n.to_string()), ops, models, *action, *answer)
            })
            .collect()
    }
}

/// What came of `action`, and the op replies and model responses the fake sandbox heard, if any.
///
/// # Panics
///
/// Panics unless `action` returns within [`HUNG`], which only an action that hangs takes.
fn run(
    dir: &Path,
    ops: Option<OpPayloadLimit>,
    models: bool,
    action: Action,
    answer: Answer,
) -> String {
    let (mut session, arrivals, sandbox) = connect(dir, ops, models, answer);
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

/// A run's sockets, made at `dir`, where nothing is yet, the registration of a sandbox's socket
/// in them, and the socket's path.
fn issued(dir: &Path) -> (Sockets, Registration, PathBuf) {
    let sockets = Sockets::create(dir.to_owned()).unwrap();
    let name = Name::try_from("sandbox".to_owned()).unwrap();
    let (SocketDial { dir }, registration) = sockets.issue(&name).unwrap();
    (sockets, registration, dir.join(SOCKET_NAME))
}

/// A session, accepted with `ops` and `models`, with a fake sandbox in the new directory `dir`
/// that answers the session's first request with `answer`'s frames, what arrives for the
/// session's owner, and the sandbox's thread, which returns the op replies and model responses
/// it heard.
fn connect(
    dir: &Path,
    ops: Option<OpPayloadLimit>,
    models: bool,
    answer: Answer,
) -> (Session, Receiver<Arrival>, JoinHandle<Vec<String>>) {
    fs::create_dir(dir).unwrap();
    let (_sockets, registration, socket) = issued(&dir.join("sockets"));
    let sandbox_socket = socket.clone();
    let sandbox = thread::spawn(move || {
        let mut connection = UnixStream::connect(sandbox_socket).unwrap();
        frame::write(&mut connection, &hello(None), &[]).unwrap();
        let reply = frame::read::<HelloReply>(&mut connection).unwrap();
        let reply = reply.map(|(reply, _)| reply);
        assert_eq!(reply, Some(HelloReply::Accepted), "the answer to the hello");
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
            } else if message == "stall" {
                // Here and below, it reads nothing more, as a stuck sandbox: what the
                // connection's buffers hold by the hang-up depends on their sizes.
                send_unended(&mut connection);
                return replies;
            } else if message == "hang_up_unread" {
                // A byte alone, so that the rest of the frame is unread as the connection closes.
                connection.read_exact(&mut [0]).unwrap();
                return replies;
            } else if let Some(message) = message.get("until_hang_up") {
                await_hang_up(&mut connection, message, &body);
                return replies;
            } else {
                frame::write(&mut connection, &message, &body).is_ok()
            };
            // The gateway may hang up at any frame, leaving the rest unheard.
            if !went_on {
                break;
            }
        }
        // End the session, as a sandbox that closes its end of the connection does, but note
        // the op replies and model responses until the gateway closes its end too. A connection
        // the gateway has closed already is fine.
        let _ = connection.shutdown(Shutdown::Write);
        while hear_reply(&mut connection, &mut replies) {}
        replies
    });
    let (sender, arrivals) = mpsc::channel();
    let session = registration
        .accept(hung(), ops, models, sender, |arrival| arrival, || false)
        .unwrap();
    let second = UnixStream::connect(&socket).unwrap_err().kind();
    assert_eq!(second, std::io::ErrorKind::NotFound, "a second connection");
    (session, arrivals, sandbox)
}

/// A fake sandbox in the new directory `dir` that connects and sends `opening` before the accept
/// begins, so that the accept, by `by`, takes its connection at once: what the sandbox heard in
/// answer, then what came of the accept, and of a freeze, which the sandbox answers, if the
/// session opened.
fn opened(dir: &Path, opening: Opening, by: Deadline) -> String {
    let (_sockets, registration, socket) = issued(dir);
    let mut connection = UnixStream::connect(socket).unwrap();
    opening(&mut connection);
    let sandbox = thread::spawn(move || {
        // It waits for the gateway no longer than a test that hangs would.
        connection.set_read_timeout(Some(HUNG)).unwrap();
        let answer = match frame::read::<Value>(&mut connection) {
            Ok(Some((answer, _))) => answer.to_string(),
            Ok(None) => "nothing".to_owned(),
            Err(error) => format!("{:?}: {error}", error.kind()),
        };
        if let Ok(Some((request, _))) = frame::read::<ToSandbox>(&mut connection) {
            let (message, body) = done(stream_of(&request).unwrap());
            frame::write(&mut connection, &message, &body).unwrap();
        }
        answer
    });
    let (sender, _arrivals) = mpsc::channel();
    let accepted = registration.accept(by, None, false, sender, |arrival| arrival, || false);
    let outcome = outcome(accepted.and_then(|mut session| session.freeze(hung())));
    format!("heard {}; {outcome}", sandbox.join().unwrap())
}

/// `outcome`, and the op replies and model responses that `sandbox` heard by the end of
/// `session`, if any.
///
/// # Panics
///
/// Panics unless dropping `session` returns within [`HUNG`], by when its reader has sent
/// whatever was left to arrive, and stopped.
fn heard(
    outcome: String,
    session: Session,
    arrivals: Receiver<Arrival>,
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

/// Reads what the gateway sends until an op's reply or a model response's end, noting each in
/// `replies` with the parts of model responses before it: `false` if the session ends first.
fn hear_reply(connection: &mut UnixStream, replies: &mut Vec<String>) -> bool {
    while let Ok(Some((message, body))) = frame::read::<ToSandbox>(connection) {
        let answered = matches!(
            message,
            ToSandbox::OpReply { .. } | ToSandbox::ModelEnd { .. }
        );
        replies.extend(note(message, &body));
        if answered {
            return true;
        }
    }
    false
}

/// An op's reply or a part of a model response, as the fake sandbox notes it.
fn note(message: ToSandbox, body: &[u8]) -> Option<String> {
    match message {
        ToSandbox::OpReply { op, reply } => Some(format!(
            "op {op}: {}",
            serde_json::to_string(&reply).unwrap()
        )),
        ToSandbox::ModelResponse {
            request,
            status,
            headers,
        } => Some(format!("model request {request}: {status} {headers:?}")),
        ToSandbox::ModelData { request } => Some(format!(
            "model request {request}: {:?}",
            String::from_utf8_lossy(body)
        )),
        ToSandbox::ModelEnd { request, cut } => Some(format!(
            "model request {request}: {}",
            if cut { "cut" } else { "end" }
        )),
        ToSandbox::Put { .. }
        | ToSandbox::Entry { .. }
        | ToSandbox::Data { .. }
        | ToSandbox::End { .. }
        | ToSandbox::Start { .. }
        | ToSandbox::Freeze { .. }
        | ToSandbox::Collect { .. } => None,
    }
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

/// Sends a frame a byte at a time, a millisecond apart, and reads nothing, until the gateway
/// hangs up, which a write then shows by failing. The frame's body is a full one, whose last byte
/// is never sent, so the gateway reads no whole frame to act on.
///
/// # Panics
///
/// Panics if the gateway is still there after [`HUNG`].
fn send_unended(connection: &mut UnixStream) {
    let mut unended = Vec::new();
    let (message, _) = done(0);
    frame::write(&mut unended, &message, &vec![0; frame::MAX_BODY_BYTES]).unwrap();
    unended.pop();
    let hung = hung();
    for byte in unended {
        if connection.write_all(&[byte]).is_err() {
            return;
        }
        assert!(
            hung.remaining().is_some(),
            "the gateway was still there after {HUNG:?}"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

/// The stream a request names: `None` for an op's reply or a part of a model response.
fn stream_of(request: &ToSandbox) -> Option<u32> {
    match *request {
        ToSandbox::Put { stream, .. }
        | ToSandbox::Entry { stream, .. }
        | ToSandbox::Data { stream }
        | ToSandbox::End { stream }
        | ToSandbox::Start { stream, .. }
        | ToSandbox::Freeze { stream }
        | ToSandbox::Collect { stream, .. } => Some(stream),
        ToSandbox::OpReply { .. }
        | ToSandbox::ModelResponse { .. }
        | ToSandbox::ModelData { .. }
        | ToSandbox::ModelEnd { .. } => None,
    }
}

/// Collects `/out` within small limits, and lists the files that arrived.
fn collect(session: &mut Session, _: &Receiver<Arrival>, dir: &Path) -> String {
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
        stdin: None,
        stdout: path("/stdout"),
        stderr: path("/stderr"),
    }
}

/// Starts a command and, while it runs, freezes the sandbox.
fn start_and_freeze(session: &mut Session, _: &Receiver<Arrival>, _: &Path) -> String {
    let process = session.start(launch(), hung());
    let frozen = session.freeze(hung());
    format!(
        "start: {}; freeze: {}",
        outcome(process.map(drop)),
        outcome(frozen)
    )
}

/// Asks for an op payload limit over the protocol's, which is refused, then answers ops as
/// [`freeze_and_take`] does.
fn over_the_cap(session: &mut Session, arrivals: &Receiver<Arrival>, dir: &Path) -> String {
    let refused = OpPayloadLimit::try_from(MAX_OP_PAYLOAD_BYTES + 1).unwrap_err();
    format!(
        "op payload limit: {refused}; {}",
        freeze_and_take(session, arrivals, dir)
    )
}

/// Freezes the sandbox, whose answer may carry ops and model requests, then takes what arrives
/// ([`take`]).
fn freeze_and_take(session: &mut Session, arrivals: &Receiver<Arrival>, _: &Path) -> String {
    let frozen = outcome(session.freeze(hung()));
    format!("freeze: {frozen}; {}", take(session, arrivals, None))
}

/// Starts a command, whose stream carries what the fake sandbox answers, then takes what
/// arrives ([`take`]), whether it started or not.
fn start_and_take(session: &mut Session, arrivals: &Receiver<Arrival>, _: &Path) -> String {
    match session.start(launch(), hung()) {
        Ok(process) => format!("started; {}", take(session, arrivals, Some(process))),
        Err(error) => format!(
            "start: {}; {}",
            outcome(Err(error)),
            take(session, arrivals, None)
        ),
    }
}

/// Collects `/out` as [`collect`] does, then takes what arrives ([`take`]).
fn collect_and_take(session: &mut Session, arrivals: &Receiver<Arrival>, dir: &Path) -> String {
    let collected = collect(session, arrivals, dir);
    format!("{collected}; {}", take(session, arrivals, None))
}

/// Answers each op that arrives, refuses each whose payload was over the limit, answers each
/// model request as [`answer_model`] does, and notes each command's exit, saying whether it is
/// `started`'s, until the session's end, and then anything that arrives after it.
fn take(session: &mut Session, arrivals: &Receiver<Arrival>, started: Option<Process>) -> String {
    let mut lines = Vec::new();
    loop {
        let Some(arrival) = next(arrivals) else {
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
            Arrival::ModelRequest(incoming) => lines.push(answer_model(incoming)),
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
    if let Some(after) = next(arrivals) {
        lines.push(format!("then {after:?}"));
    }
    lines.join("; ")
}

/// The next arrival, or `None` once the session's reader has stopped.
///
/// # Panics
///
/// Panics if nothing arrives within [`HUNG`].
fn next(arrivals: &Receiver<Arrival>) -> Option<Arrival> {
    match arrivals.recv_timeout(HUNG) {
        Ok(arrival) => Some(arrival),
        Err(RecvTimeoutError::Disconnected) => None,
        Err(RecvTimeoutError::Timeout) => panic!("nothing arrived within {HUNG:?}"),
    }
}

/// The next arrival, which must be an op that arrived whole.
fn next_op(arrivals: &Receiver<Arrival>) -> IncomingOp {
    match next(arrivals) {
        Some(Arrival::Op(op)) => op,
        arrived => panic!("an op was to arrive, not {arrived:?}"),
    }
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
fn answer_elsewhere(session: &mut Session, arrivals: &Receiver<Arrival>, dir: &Path) -> String {
    let ops = Some(OpPayloadLimit::NONE);
    let (mut other, others, sandbox) = connect(&dir.join("other"), ops, false, |s| {
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
            (next_op(arrivals), process)
        });
    let message = "from another session".to_owned();
    let elsewhere = panicked(|| other.reply(mine, OpReply::Refused { message }, hung()));
    let own = outcome(other.reply(own, no_grants(), hung()));
    let refused = match next(&others) {
        Some(Arrival::Refused(refused)) => refused,
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
fn too_long_reply(session: &mut Session, arrivals: &Receiver<Arrival>, _: &Path) -> String {
    let mut lines = vec![format!("freeze: {}", outcome(session.freeze(hung())))];
    for message in ["x".repeat(frame::MAX_MESSAGE_BYTES), "no".to_owned()] {
        let op = next_op(arrivals);
        lines.push(outcome(session.reply(
            op,
            OpReply::Refused { message },
            hung(),
        )));
    }
    lines.join("; ")
}

/// Answers `incoming` as its path says, and notes it and what came of the answer: `/cut` and
/// `/drop` with a response cut short and dropped after its first part, `/long` with a head too
/// long for a frame, `/late` by a deadline that has passed, `/stale` with a body larger than the
/// connection's buffers hold by a [`short`] deadline, and any other with the response whole, in
/// two parts.
fn answer_model(incoming: IncomingModelRequest) -> String {
    let request = &incoming.request;
    let body = String::from_utf8_lossy(&request.body);
    let line = format!(
        "{} {} {:?} {body:?}",
        request.method, request.path, request.headers
    );
    let path = request.path.clone();
    let mut headers = vec![("content-type".to_owned(), "text/plain".to_owned())];
    let by = match path.as_str() {
        "/late" => Deadline::after(Duration::ZERO),
        "/stale" => short(),
        "/long" => {
            headers.push(("x-long".to_owned(), "x".repeat(frame::MAX_MESSAGE_BYTES)));
            hung()
        }
        _ => hung(),
    };
    let answered = incoming.respond(200, headers, by).and_then(|mut response| {
        if path == "/stale" {
            return response.send(&vec![0; 8 << 20]);
        }
        response.send(b"hel")?;
        match path.as_str() {
            "/cut" => response.cut(),
            "/drop" => {
                drop(response);
                Ok(())
            }
            _ => response.send(b"lo").and_then(|()| response.end()),
        }
    });
    format!("{line}: {}", outcome(answered))
}

/// Answers the model request that the sandbox of a session of its own makes on another thread,
/// with a body larger than the connection's buffers hold by a deadline only a hang reaches,
/// while that sandbox reads nothing; then, while the answer waits on it, drops the session, which
/// fails the answer at once, for the session's end.
fn answer_on_a_thread(_: &mut Session, _: &Receiver<Arrival>, dir: &Path) -> String {
    let (mut session, arrivals, sandbox) = connect(&dir.join("other"), None, true, |s| {
        vec![done(s), model_request(3, "/", 0), stall()]
    });
    session.freeze(hung()).unwrap();
    let Some(Arrival::ModelRequest(incoming)) = next(&arrivals) else {
        panic!("a model request was to arrive");
    };
    let answer = thread::spawn(move || {
        let mut response = incoming.respond(200, Vec::new(), hung())?;
        response.send(&vec![0; 8 << 20])
    });
    // Time for the answer to fill the buffers and wait on the sandbox, so that the session ends
    // under it. Whenever it ends, the answer fails for the session's end.
    thread::sleep(PAUSE);
    drop(session);
    let answered = outcome(answer.join().unwrap());
    sandbox.join().unwrap();
    format!("an answer on another thread, as its session ends: {answered}")
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
fn after_the_end(session: &mut Session, _: &Receiver<Arrival>, _: &Path) -> String {
    let ended = freezes(session, hung());
    let late = outcome(session.freeze(Deadline::after(Duration::ZERO)));
    format!("{ended}; then by a passed deadline {late}")
}

/// Puts a file larger than the connection's buffers hold, by `by`.
fn put_large(session: &mut Session, by: Deadline) -> std::io::Result<()> {
    let dir = AbsolutePath::try_from("/in".to_owned()).unwrap();
    let file = RelativePath::try_from("big".to_owned()).unwrap();
    let bytes = vec![0; 8 << 20];
    session.put(&dir, &[Put::File(&file, file_mode(), &bytes)], by)
}

/// Freezes the sandbox, whose answer carries an op, answers the op by a deadline that has
/// passed, and freezes it again ([`then_freeze`]).
fn late_reply(session: &mut Session, arrivals: &Receiver<Arrival>, _: &Path) -> String {
    session.freeze(hung()).unwrap();
    let op = next_op(arrivals);
    let replied = session.reply(op, no_grants(), Deadline::after(Duration::ZERO));
    then_freeze(session, replied)
}

/// Collects `/out` by a [`short`] deadline, within limits no tree reaches.
fn collect_short(session: &mut Session, _: &Receiver<Arrival>, dir: &Path) -> String {
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

/// Opens model request `number`, a `POST` to `path` whose body is `size` bytes.
fn model_request(number: u32, path: &str, size: u64) -> (Value, Vec<u8>) {
    let message = json!({
        "type": "model_request",
        "request": number,
        "method": "POST",
        "path": path,
        "headers": [["content-type", "application/json"]],
        "size": size,
    });
    (message, Vec::new())
}

fn model_data(number: u32, bytes: &[u8]) -> (Value, Vec<u8>) {
    let message = json!({ "type": "model_data", "request": number });
    (message, bytes.to_vec())
}

/// The numbers of as many model requests as may be in flight at once, from 0.
fn flight() -> Range<u32> {
    0..u32::try_from(MAX_MODEL_REQUESTS_IN_FLIGHT).unwrap()
}

/// Not a frame: where the fake sandbox waits for the gateway's next answer: the reply to the op
/// in flight, as `sandboxd` waits before it opens another, or a model response's end.
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

/// Not a frame: where the fake sandbox reads and answers nothing until the gateway hangs up, as a
/// stuck sandbox does ([`send_unended`]).
fn stall() -> (Value, Vec<u8>) {
    (Value::from("stall"), Vec::new())
}

/// Not a frame: where the fake sandbox closes the connection once the gateway's next frame has
/// come, with all of it but its first byte unread, as a sandbox that exits does, which resets
/// the connection rather than ends it cleanly.
fn hang_up_unread() -> (Value, Vec<u8>) {
    (Value::from("hang_up_unread"), Vec::new())
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
        fakes(
            None,
            false,
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
                // A malformed tree ends the session, which the sandbox leaves open, stalling, so
                // that the gateway's end comes first; a tree that passes a limit fails only its
                // collect.
                (collect, |s| vec![dir(s, "a/a/a"), stall()]),
                (collect, |s| vec![file(s, "a/b", 0), stall()]),
                (collect, |s| vec![file(s, "a", 0), dir(s, "a/b"), stall()]),
                (collect, |s| vec![file(s, "a", 0), dir(s, "a"), stall()]),
                (collect, |s| vec![data(s, b"x"), stall()]),
                (collect, |s| vec![file(s, "big", 9)]),
                (collect, |s| {
                    ["a", "b", "c", "d"].map(|path| dir(s, path)).to_vec()
                }),
                (collect, |s| {
                    vec![file(s, "f", 1), data(s, b"too long"), stall()]
                }),
                (collect, |s| {
                    vec![
                        file(s, "f", 1),
                        data(s, b""),
                        data(s, b"x"),
                        end(s),
                        stall(),
                    ]
                }),
                (collect, |s| vec![file(s, "f", 2), end(s), stall()]),
                (collect, |s| vec![reply("failed", s)]),
                // A started command's stream carries only its exit, so a second `done` breaks the
                // protocol.
                (start_and_freeze, |s| vec![done(s); 2]),
                // The command's route goes at its exit, its stream's last frame, so later ones
                // drop.
                (start_and_freeze, |s| {
                    let mut flood = vec![done(s)];
                    flood.extend((0..3).map(|_| reply("exited", s)));
                    flood
                }),
                // Closing with a request unread resets the connection: the sandbox ended it too.
                (after_the_end, |s| vec![done(s), hang_up_unread()]),
            ],
        ),
        &expect![[r#"
            ok; arrived ["a/b=hi"]
            ConnectionAborted: the session failed: `../escape` is not a normalized relative path: use `/`-separated components, none of them empty, `.` or `..`, and no NUL byte; arrived []
            ConnectionAborted: the session failed: `/etc/x` is not a normalized relative path: use `/`-separated components, none of them empty, `.` or `..`, and no NUL byte; arrived []
            ConnectionAborted: the session failed: unknown variant `link`, expected `dir` or `file`; arrived []
            ConnectionAborted: the sandbox broke the protocol: the tree it sent on stream 1 is malformed: a/a/a: No such file or directory (os error 2); arrived []
            ConnectionAborted: the sandbox broke the protocol: the tree it sent on stream 1 is malformed: a/b: No such file or directory (os error 2); arrived []
            ConnectionAborted: the sandbox broke the protocol: the tree it sent on stream 1 is malformed: a/b: Not a directory (os error 20); arrived ["a="]
            ConnectionAborted: the sandbox broke the protocol: the tree it sent on stream 1 is malformed: a: File exists (os error 17); arrived ["a="]
            ConnectionAborted: the sandbox broke the protocol: the tree it sent on stream 1 is malformed: file data arrived outside a file: send it after a file entry; arrived []
            FileTooLarge: the tree's files hold more than 8 bytes; arrived []
            FileTooLarge: the tree has more than 3 entries; arrived ["a/", "b/", "c/"]
            ConnectionAborted: the sandbox broke the protocol: the tree it sent on stream 1 is malformed: file data runs past the file's size: send exactly the size given; arrived ["f="]
            ConnectionAborted: the sandbox broke the protocol: the tree it sent on stream 1 is malformed: file data is empty: send at least one byte in each frame; arrived ["f="]
            ConnectionAborted: the sandbox broke the protocol: the tree it sent on stream 1 is malformed: a file ended 2 bytes short: send all of its bytes before the next part; arrived ["f="]
            Other: no; arrived []
            start: ok; freeze: ConnectionAborted: the sandbox broke the protocol: it sent more on stream 1 than a request takes
            start: ok; freeze: ConnectionAborted: the sandbox ended the session
            ok; then ConnectionAborted: the sandbox ended the session; then by a passed deadline ConnectionAborted: the sandbox ended the session"#]],
    );
}

#[test]
fn protocol_breaks() {
    check(
        fakes(
            None,
            false,
            &[
                (freeze_and_take, |s| vec![end(s), done(s)]),
                (start_and_take, |s| vec![reply("exited", s), done(s)]),
                (collect_and_take, |s| vec![dir(s, "a"), done(s), end(s)]),
                (freeze_and_take, |s| vec![done(s + 1), done(s)]),
                (freeze_and_take, |s| vec![done(0), done(s)]),
                (collect_and_take, |s| vec![dir(s, "a/b"), stall()]),
                (collect_and_take, |s| {
                    vec![
                        file(s, "big", 9),
                        data(s, b"12345"),
                        data(s, b"6789"),
                        end(s),
                    ]
                }),
            ],
        ),
        &expect![[r#"
            freeze: ConnectionAborted: the sandbox broke the protocol: it sent `end` on stream 1 out of turn: expected `done` or `failed`; ended: the sandbox broke the protocol: it sent `end` on stream 1 out of turn: expected `done` or `failed`
            start: ConnectionAborted: the sandbox broke the protocol: it sent `exited` on stream 1 out of turn: expected `done` or `failed`; ended: the sandbox broke the protocol: it sent `exited` on stream 1 out of turn: expected `done` or `failed`
            ConnectionAborted: the sandbox broke the protocol: it sent `done` on stream 1 out of turn: expected `entry`, `data`, `end` or `failed`; arrived ["a/"]; ended: the sandbox broke the protocol: it sent `done` on stream 1 out of turn: expected `entry`, `data`, `end` or `failed`
            freeze: ConnectionAborted: the sandbox broke the protocol: it sent `done` on stream 2 out of turn: expected nothing on a stream no request has opened; ended: the sandbox broke the protocol: it sent `done` on stream 2 out of turn: expected nothing on a stream no request has opened
            freeze: ConnectionAborted: the sandbox broke the protocol: it sent `done` on stream 0 out of turn: expected nothing on a stream no request has opened; ended: the sandbox broke the protocol: it sent `done` on stream 0 out of turn: expected nothing on a stream no request has opened
            ConnectionAborted: the sandbox broke the protocol: the tree it sent on stream 1 is malformed: a/b: No such file or directory (os error 2); arrived []; ended: the sandbox broke the protocol: the tree it sent on stream 1 is malformed: a/b: No such file or directory (os error 2)
            FileTooLarge: the tree's files hold more than 8 bytes; arrived []; ended: the sandbox ended the session"#]],
    );
}

#[test]
fn sandbox_ops() {
    check(
        fakes(
            Some(OpPayloadLimit::try_from(4).unwrap()),
            false,
            &[
                // A payload over the limit is refused once all of its bytes have arrived, and the
                // next op follows; an op without a payload is whole at once.
                (freeze_and_take, |s| {
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
                (freeze_and_take, |s| vec![done(s), op_data(7, b"x")]),
                (freeze_and_take, |s| {
                    vec![done(s), train(7, MAX_OP_PAYLOAD_BYTES + 1)]
                }),
                (freeze_and_take, |s| {
                    vec![done(s), op(9, json!({ "kind": "status", "job": 3 }), 3)]
                }),
                (freeze_and_take, |s| {
                    vec![done(s), train(7, 2), op_data(7, b"abc")]
                }),
                // An op over the limit is in flight from its `Op`, while its bytes arrive and
                // before its refusal, which goes only after its last byte: so opening another
                // breaks the protocol, and nothing is handed out.
                (freeze_and_take, |s| {
                    let ops = json!({ "kind": "ops" });
                    vec![done(s), train(7, 5), op_data(7, b"abcd"), op(8, ops, 0)]
                }),
                // Data for an op whose payload has all arrived.
                (freeze_and_take, |_| {
                    vec![train(7, 1), op_data(7, b"a"), op_data(7, b"x")]
                }),
                // An op is in flight until answered, and the session ends before the freeze is
                // answered, with the op queued.
                (freeze_and_take, |_| {
                    vec![train(7, 0), op(8, json!({ "kind": "ops" }), 0)]
                }),
                // An op over the limit is handed out refused though the session ended before the
                // refusal could be sent.
                (freeze_and_take, |_| {
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
        ),
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
}

#[test]
fn sandbox_model_requests() {
    check(
        fakes(
            None,
            true,
            &[
                // A body in two parts, and none.
                (freeze_and_take, |s| {
                    let body = [br#"{"a""#.as_slice(), b":1}"].map(|part| model_data(3, part));
                    let mut frames = vec![done(s), model_request(3, "/", 7)];
                    frames.extend(body);
                    frames.extend([model_request(4, "/", 0), await_reply(), await_reply()]);
                    frames
                }),
                // Responses cut short, dropped, and too long to start, which is cut short too and
                // leaves the session open; then one started too late, which ends it.
                (freeze_and_take, |s| {
                    let mut frames = vec![done(s)];
                    for (n, path) in (3..).zip(["/cut", "/drop", "/long", "/late"]) {
                        frames.extend([model_request(n, path, 0), await_reply()]);
                    }
                    frames
                }),
                (freeze_and_take, |s| {
                    vec![done(s), model_request(3, "/", 1), model_request(3, "/", 1)]
                }),
                // Data for a model request never opened, and for one whose body has all arrived.
                (freeze_and_take, |s| vec![done(s), model_data(3, b"x")]),
                (freeze_and_take, |_| {
                    vec![
                        model_request(3, "/", 1),
                        model_data(3, b"a"),
                        model_data(3, b"x"),
                    ]
                }),
                (freeze_and_take, |s| {
                    vec![done(s), model_request(3, "/stale", 0), stall()]
                }),
                (answer_on_a_thread, |_| Vec::new()),
                (freeze_and_take, |s| {
                    let (mut open, body) = model_request(3, "/", 0);
                    open["headers"] = json!([["content-type", "x"], ["X-Key", "y"]]);
                    vec![done(s), (open, body)]
                }),
                (freeze_and_take, |s| {
                    vec![done(s), model_request(3, "/", MAX_MODEL_REQUEST_BYTES + 1)]
                }),
                (freeze_and_take, |s| {
                    vec![done(s), model_request(3, "/", 2), model_data(3, b"abc")]
                }),
            ],
        ),
        &expect![[r#"
            freeze: ok; POST / [("content-type", "application/json")] "{\"a\":1}": ok; POST / [("content-type", "application/json")] "": ok; ended: the sandbox ended the session; the sandbox heard model request 3: 200 [("content-type", "text/plain")], model request 3: "hel", model request 3: "lo", model request 3: end, model request 4: 200 [("content-type", "text/plain")], model request 4: "hel", model request 4: "lo", model request 4: end
            freeze: ok; POST /cut [("content-type", "application/json")] "": ok; POST /drop [("content-type", "application/json")] "": ok; POST /long [("content-type", "application/json")] "": InvalidInput: the message is 65642 bytes: a frame's message holds at most 65536; POST /late [("content-type", "application/json")] "": TimedOut: the response's deadline passed before it was sent: expected time left to send it; ended: the response's deadline passed before it was sent: expected time left to send it; the sandbox heard model request 3: 200 [("content-type", "text/plain")], model request 3: "hel", model request 3: cut, model request 4: 200 [("content-type", "text/plain")], model request 4: "hel", model request 4: cut, model request 5: cut
            freeze: ok; ended: the sandbox broke the protocol: it opened model request 3, which is in flight
            freeze: ok; ended: the sandbox broke the protocol: it sent data for model request 3, which is not open
            freeze: ConnectionAborted: the sandbox broke the protocol: it sent data for model request 3, which is not open; POST / [("content-type", "application/json")] "a": ConnectionAborted: the sandbox broke the protocol: it sent data for model request 3, which is not open; ended: the sandbox broke the protocol: it sent data for model request 3, which is not open
            freeze: ok; POST /stale [("content-type", "application/json")] "": TimedOut: the sandbox didn't take what was sent before the response's deadline: expected it to read while the deadline lasts; ended: the sandbox didn't take what was sent before the response's deadline: expected it to read while the deadline lasts
            an answer on another thread, as its session ends: ConnectionAborted: the owner dropped the session
            freeze: ok; ended: the sandbox broke the protocol: it opened model request 3 with a header name that is not in lowercase
            freeze: ok; ended: the sandbox broke the protocol: it opened model request 3 with a body of 67108865 bytes, more than the protocol's 67108864
            freeze: ok; ended: the sandbox broke the protocol: it sent 3 bytes of model request 3's body, past its size: 2 were left"#]],
    );
}

#[test]
fn model_flight() {
    check(
        fakes(
            None,
            true,
            &[
                // A full flight, then one more once their responses have ended whole.
                (freeze_and_take, |s| {
                    let mut frames: Vec<_> = flight().map(|n| model_request(n, "/", 0)).collect();
                    frames.push(done(s));
                    frames.extend(flight().map(|_| await_reply()));
                    frames.extend([model_request(99, "/", 0), await_reply()]);
                    frames
                }),
                // A full flight, then one more once their responses have ended cut short, dropped,
                // or cut short as their heads are too long to send.
                (freeze_and_take, |s| {
                    let paths = ["/cut", "/drop", "/long"];
                    let mut frames: Vec<_> = flight()
                        .zip(paths.into_iter().cycle())
                        .map(|(n, path)| model_request(n, path, 0))
                        .collect();
                    frames.push(done(s));
                    frames.extend(flight().map(|_| await_reply()));
                    frames.extend([model_request(99, "/", 0), await_reply()]);
                    frames
                }),
                // One past a full flight, each of whose requests is whole, before the owner can
                // take any: the session ends, and then each answer fails for its end.
                (freeze_and_take, |s| {
                    let mut frames: Vec<_> = flight().map(|n| model_request(n, "/", 0)).collect();
                    frames.extend([model_request(99, "/", 0), done(s)]);
                    frames
                }),
                // One past a full flight, whose requests' bodies are still arriving.
                (freeze_and_take, |s| {
                    let mut frames = vec![done(s)];
                    frames.extend(flight().chain([99]).map(|n| model_request(n, "/", 1)));
                    frames
                }),
            ],
        ),
        &expect![[r#"
            freeze: ok; POST / [("content-type", "application/json")] "": ok; POST / [("content-type", "application/json")] "": ok; POST / [("content-type", "application/json")] "": ok; POST / [("content-type", "application/json")] "": ok; POST / [("content-type", "application/json")] "": ok; POST / [("content-type", "application/json")] "": ok; POST / [("content-type", "application/json")] "": ok; POST / [("content-type", "application/json")] "": ok; ended: the sandbox ended the session; the sandbox heard model request 0: 200 [("content-type", "text/plain")], model request 0: "hel", model request 0: "lo", model request 0: end, model request 1: 200 [("content-type", "text/plain")], model request 1: "hel", model request 1: "lo", model request 1: end, model request 2: 200 [("content-type", "text/plain")], model request 2: "hel", model request 2: "lo", model request 2: end, model request 3: 200 [("content-type", "text/plain")], model request 3: "hel", model request 3: "lo", model request 3: end, model request 4: 200 [("content-type", "text/plain")], model request 4: "hel", model request 4: "lo", model request 4: end, model request 5: 200 [("content-type", "text/plain")], model request 5: "hel", model request 5: "lo", model request 5: end, model request 6: 200 [("content-type", "text/plain")], model request 6: "hel", model request 6: "lo", model request 6: end, model request 99: 200 [("content-type", "text/plain")], model request 99: "hel", model request 99: "lo", model request 99: end
            freeze: ok; POST /cut [("content-type", "application/json")] "": ok; POST /drop [("content-type", "application/json")] "": ok; POST /long [("content-type", "application/json")] "": InvalidInput: the message is 65642 bytes: a frame's message holds at most 65536; POST /cut [("content-type", "application/json")] "": ok; POST /drop [("content-type", "application/json")] "": ok; POST /long [("content-type", "application/json")] "": InvalidInput: the message is 65642 bytes: a frame's message holds at most 65536; POST /cut [("content-type", "application/json")] "": ok; POST / [("content-type", "application/json")] "": ok; ended: the sandbox ended the session; the sandbox heard model request 0: 200 [("content-type", "text/plain")], model request 0: "hel", model request 0: cut, model request 1: 200 [("content-type", "text/plain")], model request 1: "hel", model request 1: cut, model request 2: cut, model request 3: 200 [("content-type", "text/plain")], model request 3: "hel", model request 3: cut, model request 4: 200 [("content-type", "text/plain")], model request 4: "hel", model request 4: cut, model request 5: cut, model request 6: 200 [("content-type", "text/plain")], model request 6: "hel", model request 6: cut, model request 99: 200 [("content-type", "text/plain")], model request 99: "hel", model request 99: "lo", model request 99: end
            freeze: ConnectionAborted: the sandbox broke the protocol: it opened model request 99 while 7 are in flight, the most the protocol allows; POST / [("content-type", "application/json")] "": ConnectionAborted: the sandbox broke the protocol: it opened model request 99 while 7 are in flight, the most the protocol allows; POST / [("content-type", "application/json")] "": ConnectionAborted: the sandbox broke the protocol: it opened model request 99 while 7 are in flight, the most the protocol allows; POST / [("content-type", "application/json")] "": ConnectionAborted: the sandbox broke the protocol: it opened model request 99 while 7 are in flight, the most the protocol allows; POST / [("content-type", "application/json")] "": ConnectionAborted: the sandbox broke the protocol: it opened model request 99 while 7 are in flight, the most the protocol allows; POST / [("content-type", "application/json")] "": ConnectionAborted: the sandbox broke the protocol: it opened model request 99 while 7 are in flight, the most the protocol allows; POST / [("content-type", "application/json")] "": ConnectionAborted: the sandbox broke the protocol: it opened model request 99 while 7 are in flight, the most the protocol allows; POST / [("content-type", "application/json")] "": ConnectionAborted: the sandbox broke the protocol: it opened model request 99 while 7 are in flight, the most the protocol allows; ended: the sandbox broke the protocol: it opened model request 99 while 7 are in flight, the most the protocol allows
            freeze: ok; ended: the sandbox broke the protocol: it opened model request 99 while 7 are in flight, the most the protocol allows"#]],
    );
}

#[test]
fn passive_session() {
    check(
        fakes(
            None,
            false,
            &[
                (over_the_cap, |s| vec![done(s), train(7, 0)]),
                (freeze_and_take, |s| vec![done(s), model_request(3, "/", 0)]),
            ],
        ),
        &expect![[r#"
            op payload limit: the op payload limit is 67108865 bytes, over the protocol's 67108864: give at most that; freeze: ok; ended: the sandbox broke the protocol: it opened op 7, but the session takes no ops
            freeze: ok; ended: the sandbox broke the protocol: it opened model request 3, but the session takes no model requests: send one only on a session accepted to take them"#]],
    );
}

#[test]
fn another_sessions_op() {
    check(
        fakes(
            Some(OpPayloadLimit::NONE),
            false,
            &[(answer_elsewhere, |s| {
                vec![done(s), op(0, json!({ "kind": "ops" }), 0), await_reply()]
            })],
        ),
        &expect![[
            r#"the sessions' commands equal: false; op 0 on another session: panicked: internal error: entered unreachable code: op 0 was answered on a session other than the one it arrived on, though the contracts of `reply` and `refuse` require the one that handed it out; that session's own op 0: ok; its op 1 refused on another session: panicked: internal error: entered unreachable code: op 1 was answered on a session other than the one it arrived on, though the contracts of `reply` and `refuse` require the one that handed it out; the sandbox heard op 0: {"kind":"ops","remaining_seconds":0,"resources":{"cpu_millis":1,"memory_bytes":1},"grants":[]}"#
        ]],
    );
}

#[test]
fn deadlines() {
    check(
        fakes(
            Some(OpPayloadLimit::NONE),
            false,
            &[
                // The sandbox answers after a pause, or stops reading a large put for one, and then
                // reads on: the requests wait, as their deadlines haven't passed. It answers the
                // put before it has read it all, which the gateway takes once the put is all
                // written.
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
                // A deadline that has passed before a request or a reply starts, which sends
                // nothing.
                (
                    |session, _, _| freezes(session, Deadline::after(Duration::ZERO)),
                    |_| Vec::new(),
                ),
                (late_reply, |s| {
                    vec![done(s), op(0, json!({ "kind": "ops" }), 0), await_reply()]
                }),
                // A session that has ended gives its own reason, though the deadline has passed
                // too.
                (after_the_end, |s| vec![done(s)]),
                (collect, |s| vec![dir(s, "a")]),
            ],
        ),
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

#[test]
fn arrivals() {
    check(
        fakes(
            Some(OpPayloadLimit::NONE),
            true,
            &[
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
                // Model requests, each arriving once whole, around an op, and the command's exit.
                (start_and_take, |s| {
                    vec![
                        done(s),
                        model_request(3, "/a", 0),
                        model_request(4, "/b", 2),
                        op(7, json!({ "kind": "ops" }), 0),
                        model_data(4, b"hi"),
                        reply("exited", s),
                        await_reply(),
                        await_reply(),
                        await_reply(),
                    ]
                }),
                // The sandbox goes on sending, reading nothing, until the gateway hangs up.
                (
                    |session, _, _| outcome(session.freeze(hung())),
                    |s| vec![done(s), stall()],
                ),
            ],
        ),
        &expect![[r#"
            started; Ops; the command exited: Code(0); Status { job: 3 }; ended: the sandbox ended the session; the sandbox heard op 7: {"kind":"ops","remaining_seconds":0,"resources":{"cpu_millis":1,"memory_bytes":1},"grants":[]}, op 8: {"kind":"ops","remaining_seconds":0,"resources":{"cpu_millis":1,"memory_bytes":1},"grants":[]}
            started; POST /a [("content-type", "application/json")] "": ok; Ops; POST /b [("content-type", "application/json")] "hi": ok; the command exited: Code(0); ended: the sandbox ended the session; the sandbox heard model request 3: 200 [("content-type", "text/plain")], model request 3: "hel", model request 3: "lo", model request 3: end, op 7: {"kind":"ops","remaining_seconds":0,"resources":{"cpu_millis":1,"memory_bytes":1},"grants":[]}, model request 4: 200 [("content-type", "text/plain")], model request 4: "hel", model request 4: "lo", model request 4: end
            ok"#]],
    );
}

#[test]
fn sockets() {
    check(
        |dir| {
            let run = dir.join("sockets");
            let (_sockets, _registration, socket) = issued(&run);
            let sandbox = socket.parent().unwrap();
            [&run, sandbox, &socket]
                .map(|path| {
                    let mode = fs::metadata(path).unwrap().permissions().mode() & 0o7777;
                    let path = path.strip_prefix(dir).unwrap().display();
                    format!("{path}: {mode:o}")
                })
                .into()
        },
        &expect![[r#"
            sockets: 700
            sockets/sandbox: 711
            sockets/sandbox/s: 666"#]],
    );
}

#[test]
fn stopped() {
    check(
        |dir| {
            let lines = [true, false].map(|first| {
                let (_sockets, registration, socket) = issued(&dir.join(format!("{first}")));
                let _waiting = UnixStream::connect(&socket).unwrap();
                let (sender, _) = mpsc::channel();
                let by = hung();
                let stopped = || first || !socket.exists();
                let accepted = registration.accept(by, None, false, sender, |a| a, stopped);
                let returned = by.remaining().map_or("past", |_| "before");
                let accepted = outcome(accepted.map(drop));
                let accepted = accepted.replace(&dir.display().to_string(), "<dir>");
                format!("returned {returned} its deadline: {accepted}")
            });
            lines.into()
        },
        &expect![[r#"
            returned before its deadline: Other: the accept was stopped before a sandbox connected to <dir>/true/sandbox/s: expected its sandbox to connect before the stop
            returned before its deadline: Other: the accept was stopped before the sandbox's hello: expected its hello before the stop"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn hellos() {
    check(
        |dir| {
            let cases: [(&str, Opening, Deadline); 14] = [
                (
                    "this version",
                    |c| frame::write(c, &hello(None), &[]).unwrap(),
                    hung(),
                ),
                (
                    "a later version, with a field this one lacks",
                    |c| {
                        let hello = json!({ "version": PROTOCOL_VERSION + 1, "later": true });
                        frame::write(c, &hello, &[]).unwrap();
                    },
                    hung(),
                ),
                (
                    "this version, with a field it lacks",
                    |c| {
                        let hello = json!({ "version": PROTOCOL_VERSION, "later": true });
                        frame::write(c, &hello, &[]).unwrap();
                    },
                    hung(),
                ),
                (
                    "this version, with a grant",
                    |c| {
                        let grant = RegistrationGrant::try_from("07".repeat(16)).unwrap();
                        frame::write(c, &hello(Some(grant)), &[]).unwrap();
                    },
                    hung(),
                ),
                (
                    "a later version, as long as a hello may be",
                    |c| {
                        let mut hello = json!({ "version": PROTOCOL_VERSION + 1, "later": "" });
                        let pad = MAX_HELLO_BYTES - hello.to_string().len();
                        hello["later"] = json!("x".repeat(pad));
                        frame::write(c, &hello, &[]).unwrap();
                    },
                    hung(),
                ),
                (
                    "a length a byte over a hello's, and nothing more",
                    |c| {
                        let len = u32::try_from(MAX_HELLO_BYTES + 1).unwrap();
                        c.write_all(&len.to_be_bytes()).unwrap();
                    },
                    hung(),
                ),
                (
                    "this version, with a body",
                    |c| frame::write(c, &hello(None), b"x").unwrap(),
                    hung(),
                ),
                (
                    "a reply, not a hello",
                    |c| frame::write(c, &done(1).0, &[]).unwrap(),
                    hung(),
                ),
                (
                    "a frame whose message, `null`, is no object",
                    |c| frame::write(c, &Value::Null, &[]).unwrap(),
                    hung(),
                ),
                (
                    "a frame whose message, `hello`, isn't JSON",
                    |c| c.write_all(b"\0\0\0\x05hello\0\0\0\0").unwrap(),
                    hung(),
                ),
                (
                    "part of a frame, then the connection's end",
                    |c| {
                        c.write_all(b"\0\0\0\x0d{\"version\"").unwrap();
                        c.shutdown(Shutdown::Write).unwrap();
                    },
                    hung(),
                ),
                (
                    "a hello's message, then the connection's end",
                    |c| {
                        let mut written = Vec::new();
                        frame::write(&mut written, &hello(None), &[]).unwrap();
                        // All but the body's length, the last 4 bytes.
                        c.write_all(&written[..written.len() - 4]).unwrap();
                        c.shutdown(Shutdown::Write).unwrap();
                    },
                    hung(),
                ),
                (
                    "nothing, then the connection's end",
                    |c| c.shutdown(Shutdown::Write).unwrap(),
                    hung(),
                ),
                ("nothing, past the deadline", |_| {}, short()),
            ];
            (cases.into_iter().enumerate())
                .map(|(n, (name, opening, by))| {
                    let opened = opened(&dir.join(n.to_string()), opening, by);
                    format!("{name}: {opened}")
                })
                .collect()
        },
        &expect![[r#"
            this version: heard {"type":"accepted"}; ok
            a later version, with a field this one lacks: heard {"message":"sandboxd speaks sandbox protocol 1: expected 0, which this gateway speaks, as a sandboxd built from the same Pumpkin does","type":"refused"}; InvalidData: the gateway refused the session: sandboxd speaks sandbox protocol 1: expected 0, which this gateway speaks, as a sandboxd built from the same Pumpkin does
            this version, with a field it lacks: heard {"message":"the hello isn't of sandbox protocol 0's form: unknown field `later`, expected `version` or `grant`","type":"refused"}; InvalidData: the gateway refused the session: the hello isn't of sandbox protocol 0's form: unknown field `later`, expected `version` or `grant`
            this version, with a grant: heard {"message":"the hello presents a registration grant on a socket of the sandbox's own, which identifies it: expected none there","type":"refused"}; InvalidData: the gateway refused the session: the hello presents a registration grant on a socket of the sandbox's own, which identifies it: expected none there
            a later version, as long as a hello may be: heard {"message":"sandboxd speaks sandbox protocol 1: expected 0, which this gateway speaks, as a sandboxd built from the same Pumpkin does","type":"refused"}; InvalidData: the gateway refused the session: sandboxd speaks sandbox protocol 1: expected 0, which this gateway speaks, as a sandboxd built from the same Pumpkin does
            a length a byte over a hello's, and nothing more: heard {"message":"the hello is 256 bytes: a hello holds at most 255","type":"refused"}; InvalidData: the gateway refused the session: the hello is 256 bytes: a hello holds at most 255
            this version, with a body: heard {"message":"the hello has a 1-byte body: a hello has none","type":"refused"}; InvalidData: the gateway refused the session: the hello has a 1-byte body: a hello has none
            a reply, not a hello: heard {"message":"the session's first frame is no hello: missing field `version`: open the session with one","type":"refused"}; InvalidData: the gateway refused the session: the session's first frame is no hello: missing field `version`: open the session with one
            a frame whose message, `null`, is no object: heard {"message":"the session's first frame is no hello: invalid type: null, expected an object with a `version`: open the session with one","type":"refused"}; InvalidData: the gateway refused the session: the session's first frame is no hello: invalid type: null, expected an object with a `version`: open the session with one
            a frame whose message, `hello`, isn't JSON: heard {"message":"a frame's message isn't JSON: expected value at line 1 column 1","type":"refused"}; InvalidData: the gateway refused the session: a frame's message isn't JSON: expected value at line 1 column 1
            part of a frame, then the connection's end: heard nothing; UnexpectedEof: the sandbox closed the connection inside its first frame: expected a whole hello
            a hello's message, then the connection's end: heard nothing; UnexpectedEof: the sandbox closed the connection inside its first frame: expected a whole hello
            nothing, then the connection's end: heard nothing; UnexpectedEof: the sandbox closed the connection before its hello: expected it to open its session with one
            nothing, past the deadline: heard nothing; TimedOut: the sandbox connected but sent no hello by the deadline: expected its sandboxd to send one as it connects, as a sandboxd built from the same Pumpkin does"#]],
    );
}

/// The hello that presents `grant`, if any.
fn hello(grant: Option<RegistrationGrant>) -> Hello {
    Hello { grant }
}
