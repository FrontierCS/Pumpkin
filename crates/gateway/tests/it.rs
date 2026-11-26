//! The gateway's end of a session, against a fake sandbox that answers the gateway's first request
//! with frames of its choosing, ops and model requests among them, waiting where it chooses for an
//! op's reply or a model response's end, for a while or for the gateway to hang up, and then ends
//! the session: it sends what a buggy `sandboxd` might. Other fakes open their sessions with first
//! frames of their own. An accept that is stopped takes no connection. A dialer's fakes listen
//! behind the stand-in TLS edge. A companion's fakes serve TLS of their own behind a relay there,
//! which carries them or doesn't.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]

use std::cell::Cell;
use std::collections::BTreeMap;
use std::io::{Read as _, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::num::NonZeroU64;
use std::ops::Range;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};
use std::{fs, iter, panic};

use expect_test::{Expect, expect};
use gateway::{
    Arrival, Dialer, GatewayAddress, IncomingModelRequest, IncomingOp, Listener, Process, Put,
    Registration, Sockets, Unattested, Unreached,
};
use schema::frame;
use schema::ir::{AbsolutePath, Argv, CompanionName, Env, Name, Resources};
use schema::sandbox::op::{OpReply, OpsReply};
use schema::sandbox::tree::Limits;
use schema::sandbox::{
    Endpoint, HELLO_TIMEOUT, Hello, HelloReply, Launch, Listening, MAX_HELLO_BYTES,
    MAX_MODEL_REQUEST_BYTES, MAX_MODEL_REQUESTS_IN_FLIGHT, MAX_OP_PAYLOAD_BYTES, OpPayloadLimit,
    PROTOCOL_VERSION, PrivateDial, RegistrationGrant, RelativePath, Relayed, SOCKET_NAME,
    SocketDial, TlsIdentity, ToSandbox, file_mode, read_route,
};
use serde_json::{Value, json};
use stdx::Deadline;
use stdx::time::{self, Clock, Machine};
use test_support::relayed::Carried;
use test_support::{HUNG, TestDir, edge, hung, relayed};

type Session = gateway::Session<Still>;

type Action = fn(&mut Session, &Receiver<Arrival>, &Path) -> String;

type Answer = fn(stream: u32) -> Vec<(Value, Vec<u8>)>;

type Opening = fn(&mut UnixStream);

// How long the fake sandbox reads and sends nothing where its answer has a `pause`, as over a slow
// link, whose end no event marks. A request by a `hung` deadline outlasts it, so no outcome rests
// on a margin.
const PAUSE: Duration = Duration::from_millis(30);

// How long the private listener of a peer that stays silent gives it to say its hello, which is
// that peer's only outcome.
const HELLO_BOUND: Duration = Duration::from_millis(50);

// The most hellos the listener reads at once: `crowd` holds that many silent, and `turns` sends one
// more than that, each once the one before has been answered.
const HELLOS_AT_ONCE: usize = 100;

// What a case writes in its directory at its event, which a `Still` of the case reads: the fake
// sandbox's stall, or what the case's action marks.
const STALLED: &str = "stalled";

fn short() -> Duration {
    Machine.now() + Duration::from_millis(100)
}

// The thread that sends `start_late`'s start, and that looks late.
const LATE: &str = "late";

// What a `Still` reads once its event has come. A request whose deadline is `EVENT` ends on the
// event. A deadline from `hung()` is later, so no event ends it.
const EVENT: Duration = Duration::from_secs(HUNG.as_secs() / 2);

// How long a wait on a `Still` blocks before it reads the clock again, which is how it learns of
// the event.
const LOOK: Duration = Duration::from_millis(10);

// A clock that reads its origin until its event and `EVENT` from then on, so that a request ends on
// the event, not on the machine's time.
#[derive(Clone)]
struct Still(Arc<dyn Fn() -> bool + Send + Sync>);

impl Still {
    // Its event is the case's, which `marker` marks. A thread named `LATE` looks late: its first
    // read is the start's write, and its second, the wait for `done`, waits for the event.
    fn until_stalled(marker: PathBuf) -> Self {
        thread_local!(static READS: Cell<u32> = const { Cell::new(0) });
        Self(Arc::new(move || {
            let earlier = READS.replace(READS.get() + 1);
            let late = thread::current().name() == Some(LATE) && earlier == 1;
            while late && !marker.exists() {
                thread::sleep(LOOK);
            }
            marker.exists()
        }))
    }

    // Its event is `reached`, which a fake sets.
    fn until(reached: Arc<AtomicBool>) -> Self {
        Self(Arc::new(move || reached.load(Ordering::Relaxed)))
    }

    // Its event is its second read.
    fn after_a_read() -> Self {
        let reads = AtomicU32::new(0);
        Self(Arc::new(move || reads.fetch_add(1, Ordering::Relaxed) > 0))
    }
}

impl Clock for Still {
    fn now(&self) -> Duration {
        if (self.0)() { EVENT } else { Duration::ZERO }
    }

    fn realtime(&self) -> SystemTime {
        SystemTime::now()
    }

    fn look(&self) -> Option<Duration> {
        Some(LOOK)
    }
}

fn check(scenario: impl FnOnce(&Path) -> Vec<String>, expect: &Expect) {
    let dir = TestDir::new();
    let lines = scenario(dir.path());
    // The protocol's version, which a change to the protocol moves, and the next one.
    let text = (lines.join("\n"))
        .replace(
            &format!("protocol {}: expected", PROTOCOL_VERSION + 1),
            "protocol <later version>: expected",
        )
        .replace(
            &format!("expected {PROTOCOL_VERSION}, which"),
            "expected <version>, which",
        )
        .replace(
            &format!("protocol {PROTOCOL_VERSION}'s form"),
            "protocol <version>'s form",
        );
    expect.assert_eq(&text);
}

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

fn issued(dir: &Path) -> (Sockets, Registration, PathBuf) {
    let sockets = Sockets::create(dir.to_owned()).unwrap();
    let name = Name::try_from("sandbox".to_owned()).unwrap();
    let (SocketDial { dir }, registration) = sockets.issue(&name).unwrap();
    (sockets, registration, dir.join(SOCKET_NAME))
}

fn connect(
    dir: &Path,
    ops: Option<OpPayloadLimit>,
    models: bool,
    answer: Answer,
) -> (Session, Receiver<Arrival>, JoinHandle<Vec<String>>) {
    fs::create_dir(dir).unwrap();
    let (_sockets, registration, socket) = issued(&dir.join("sockets"));
    let sandbox_socket = socket.clone();
    let stalled = dir.join(STALLED);
    let clock = Still::until_stalled(stalled.clone());
    let sandbox = thread::spawn(move || {
        let stall = || fs::write(&stalled, "").unwrap();
        let mut connection = UnixStream::connect(sandbox_socket).unwrap();
        frame::write(&mut connection, &hello(), &[]).unwrap();
        let reply = frame::read::<HelloReply>(&mut connection).unwrap();
        let reply = reply.map(|(reply, _)| reply);
        assert_eq!(
            reply,
            Some(HelloReply::Accepted { grant: None }),
            "the answer to the hello"
        );
        let Some((request, _)) = frame::read::<ToSandbox>(&mut connection).unwrap() else {
            // The gateway sends no request if it ends the session first.
            return Vec::new();
        };
        let mut replies = Vec::new();
        for (message, body) in answer(stream_of(&request).unwrap()) {
            let went_on = if message.is_null() {
                hear_reply(&mut connection, &mut replies)
            } else if message == "read_frame" {
                frame::read::<ToSandbox>(&mut connection).unwrap().is_some()
            } else if message == "pause" {
                thread::sleep(PAUSE);
                true
            } else if message == "stall" {
                // Here and below, it reads nothing more, as a stuck sandbox: what the connection's
                // buffers hold by the hang-up depends on their sizes.
                stall();
                send_unended(&mut connection);
                return replies;
            } else if message == "stall_in_frame" {
                // A byte alone of a frame longer than the connection's buffers hold, so that the
                // gateway is still writing it as the sandbox stalls.
                connection.read_exact(&mut [0]).unwrap();
                stall();
                send_unended(&mut connection);
                return replies;
            } else if message == "hang_up_unread" {
                // A byte alone, so that the rest of the frame is unread as the connection closes.
                connection.read_exact(&mut [0]).unwrap();
                return replies;
            } else if let Some(message) = message.get("until_hang_up") {
                await_hang_up(&mut connection, message, &body, stall);
                return replies;
            } else {
                frame::write(&mut connection, &message, &body).is_ok()
            };
            // The gateway may hang up at any frame, leaving the rest unheard.
            if !went_on {
                break;
            }
        }
        // End the session, as a sandbox that closes its end of the connection does, but note the op
        // replies and model responses until the gateway closes its end too. A connection the
        // gateway has closed already is fine.
        let _ = connection.shutdown(Shutdown::Write);
        while hear_reply(&mut connection, &mut replies) {}
        replies
    });
    let (sender, arrivals) = mpsc::channel();
    let session = registration
        .accept((&clock, hung()), ops, models, sender, |a| a, || false)
        .unwrap();
    let second = UnixStream::connect(&socket).unwrap_err().kind();
    assert_eq!(second, std::io::ErrorKind::NotFound, "a second connection");
    (session, arrivals, sandbox)
}

fn opened(dir: &Path, opening: Opening, by: Duration) -> String {
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
    let accepted = registration.accept((&Machine, by), None, false, sender, |a| a, || false);
    let outcome = outcome(accepted.and_then(|mut session| session.freeze(hung())));
    format!("heard {}; {outcome}", sandbox.join().unwrap())
}

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
        | ToSandbox::Kill { .. }
        | ToSandbox::Freeze { .. }
        | ToSandbox::Collect { .. } => None,
    }
}

fn await_hang_up(connection: &mut UnixStream, message: &Value, body: &[u8], stall: impl Fn()) {
    let hung = Deadline::after(HUNG);
    // It stalls after four frames, one past the three parts of a tree that the gateway holds. With
    // full bodies, the reader then waits on the request's queue as the deadline passes.
    for _ in 0..4 {
        if frame::write(connection, message, body).is_err() {
            return;
        }
    }
    stall();
    while frame::write(connection, message, body).is_ok() {
        assert!(
            hung.remaining().is_some(),
            "the gateway was still there after {HUNG:?}"
        );
    }
}

fn send_unended(connection: &mut UnixStream) {
    let mut unended = Vec::new();
    let (message, _) = done(0);
    frame::write(&mut unended, &message, &vec![0; frame::MAX_BODY_BYTES]).unwrap();
    unended.pop();
    let hung = Deadline::after(HUNG);
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

fn stream_of(request: &ToSandbox) -> Option<u32> {
    match *request {
        ToSandbox::Put { stream, .. }
        | ToSandbox::Entry { stream, .. }
        | ToSandbox::Data { stream }
        | ToSandbox::End { stream }
        | ToSandbox::Start { stream, .. }
        | ToSandbox::Kill { stream, .. }
        | ToSandbox::Freeze { stream }
        | ToSandbox::Collect { stream, .. } => Some(stream),
        ToSandbox::OpReply { .. }
        | ToSandbox::ModelResponse { .. }
        | ToSandbox::ModelData { .. }
        | ToSandbox::ModelEnd { .. } => None,
    }
}

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

fn start_and_freeze(session: &mut Session, _: &Receiver<Arrival>, _: &Path) -> String {
    let process = session.start(launch(), hung());
    let frozen = session.freeze(hung());
    format!(
        "start: {}; freeze: {}",
        outcome(process.map(drop)),
        outcome(frozen)
    )
}

fn over_the_cap(session: &mut Session, arrivals: &Receiver<Arrival>, dir: &Path) -> String {
    let refused = OpPayloadLimit::try_from(MAX_OP_PAYLOAD_BYTES + 1).unwrap_err();
    format!(
        "op payload limit: {refused}; {}",
        freeze_and_take(session, arrivals, dir)
    )
}

fn freeze_and_take(session: &mut Session, arrivals: &Receiver<Arrival>, dir: &Path) -> String {
    let frozen = outcome(session.freeze(hung()));
    format!("freeze: {frozen}; {}", take(session, arrivals, dir, None))
}

fn start_and_take(session: &mut Session, arrivals: &Receiver<Arrival>, dir: &Path) -> String {
    match session.start(launch(), hung()) {
        Ok(process) => format!("started; {}", take(session, arrivals, dir, Some(process))),
        Err(error) => format!(
            "start: {}; {}",
            outcome(Err(error)),
            take(session, arrivals, dir, None)
        ),
    }
}

fn collect_and_take(session: &mut Session, arrivals: &Receiver<Arrival>, dir: &Path) -> String {
    let collected = collect(session, arrivals, dir);
    format!("{collected}; {}", take(session, arrivals, dir, None))
}

fn take(
    session: &mut Session,
    arrivals: &Receiver<Arrival>,
    dir: &Path,
    started: Option<Process>,
) -> String {
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
            Arrival::ModelRequest(incoming) => lines.push(answer_model(incoming, dir)),
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

fn next(arrivals: &Receiver<Arrival>) -> Option<Arrival> {
    match arrivals.recv_timeout(HUNG) {
        Ok(arrival) => Some(arrival),
        Err(RecvTimeoutError::Disconnected) => None,
        Err(RecvTimeoutError::Timeout) => panic!("nothing arrived within {HUNG:?}"),
    }
}

fn next_op(arrivals: &Receiver<Arrival>) -> IncomingOp {
    match next(arrivals) {
        Some(Arrival::Op(op)) => op,
        arrived => panic!("an op was to arrive, not {arrived:?}"),
    }
}

fn no_grants() -> OpReply {
    OpReply::Ops(OpsReply {
        role: Name::from_static("agent"),
        remaining_seconds: 0,
        resources: Resources {
            cpu_millis: NonZeroU64::MIN,
            memory_bytes: NonZeroU64::MIN,
        },
        grants: Vec::new(),
    })
}

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
    let elsewhere = panicked(|| other.reply(mine, OpReply::refused(message), hung()));
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

fn panicked(answer: impl FnOnce() -> std::io::Result<()> + Send) -> String {
    match thread::scope(|scope| scope.spawn(answer).join()) {
        Ok(answered) => outcome(answered),
        Err(panic) => format!("panicked: {}", panic.downcast::<String>().unwrap()),
    }
}

fn too_long_reply(session: &mut Session, arrivals: &Receiver<Arrival>, _: &Path) -> String {
    let mut lines = vec![format!("freeze: {}", outcome(session.freeze(hung())))];
    for message in ["x".repeat(frame::MAX_MESSAGE_BYTES), "no".to_owned()] {
        let op = next_op(arrivals);
        lines.push(outcome(session.reply(
            op,
            OpReply::refused(message),
            hung(),
        )));
    }
    lines.join("; ")
}

fn answer_model(incoming: IncomingModelRequest, dir: &Path) -> String {
    let request = &incoming.request;
    let body = String::from_utf8_lossy(&request.body);
    let line = format!(
        "{} {} {:?} {body:?}",
        request.method, request.path, request.headers
    );
    let path = request.path.clone();
    let mut headers = vec![("content-type".to_owned(), "text/plain".to_owned())];
    let by = match path.as_str() {
        "/late" => Duration::ZERO,
        "/stale" => EVENT,
        "/long" => {
            headers.push(("x-long".to_owned(), "x".repeat(frame::MAX_MESSAGE_BYTES)));
            hung()
        }
        _ => hung(),
    };
    let clock = Still::until_stalled(dir.join(STALLED));
    let answered = incoming
        .respond(200, headers, (&clock, by))
        .and_then(|mut response| {
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

fn answer_on_a_thread(_: &mut Session, _: &Receiver<Arrival>, dir: &Path) -> String {
    let (mut session, arrivals, sandbox) = connect(&dir.join("other"), None, true, |s| {
        vec![done(s), model_request(3, "/", 0), stall()]
    });
    session.freeze(hung()).unwrap();
    let Some(Arrival::ModelRequest(incoming)) = next(&arrivals) else {
        panic!("a model request was to arrive");
    };
    let answer = thread::spawn(move || {
        let mut response = incoming.respond(200, Vec::new(), (&Machine, hung()))?;
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

fn then_freeze(session: &mut Session, first: std::io::Result<()>) -> String {
    format!(
        "{}; then {}",
        outcome(first),
        outcome(session.freeze(hung()))
    )
}

fn freezes(session: &mut Session, by: Duration) -> String {
    let first = session.freeze(by);
    then_freeze(session, first)
}

fn after_the_end(session: &mut Session, _: &Receiver<Arrival>, _: &Path) -> String {
    let ended = freezes(session, hung());
    let late = outcome(session.freeze(Duration::ZERO));
    format!("{ended}; then by a passed deadline {late}")
}

fn put_large(session: &mut Session, by: Duration) -> std::io::Result<()> {
    let dir = AbsolutePath::try_from("/in".to_owned()).unwrap();
    let file = RelativePath::try_from("big".to_owned()).unwrap();
    // More than the socket's buffers hold, so that the put waits on a sandbox that reads nothing.
    let bytes = vec![0; 8 << 20];
    session.put(&dir, [Put::File(file, file_mode(), &bytes)], by)
}

// The put's entry waits for an op that the sandbox sends after its `done`, and the reader passes on
// frames in the order they came, so the `done` is waiting when the put looks.
fn put_after_op(session: &mut Session, arrivals: &Receiver<Arrival>, _: &Path) -> String {
    let dir = AbsolutePath::try_from("/in".to_owned()).unwrap();
    let entry = iter::once_with(|| {
        let _ = next_op(arrivals);
        let file = RelativePath::try_from("f".to_owned()).unwrap();
        Put::File(file, file_mode(), b"hi")
    });
    format!("put: {}", outcome(session.put(&dir, entry, hung())))
}

fn late_reply(session: &mut Session, arrivals: &Receiver<Arrival>, _: &Path) -> String {
    session.freeze(hung()).unwrap();
    let op = next_op(arrivals);
    let replied = session.reply(op, no_grants(), Duration::ZERO);
    then_freeze(session, replied)
}

// The sandbox answers the start, then sends an op. The owner takes the op, then marks the event,
// which passes the start's deadline before the start's thread looks.
fn start_late(session: &mut Session, arrivals: &Receiver<Arrival>, dir: &Path) -> String {
    let (_, send) = session.prepare_start(EVENT).unwrap();
    let late = thread::Builder::new().name(LATE.to_owned());
    let started = late.spawn(move || send(launch(), EVENT)).unwrap();
    let op = next_op(arrivals);
    fs::write(dir.join(STALLED), "").unwrap();
    let started = outcome(started.join().unwrap());
    let replied = outcome(session.reply(op, no_grants(), hung()));
    format!("start: {started}; reply: {replied}")
}

fn collect_until_stalled(session: &mut Session, _: &Receiver<Arrival>, dir: &Path) -> String {
    let limits = Limits {
        bytes: u64::MAX,
        entries: u64::MAX,
    };
    let out = AbsolutePath::try_from("/out".to_owned()).unwrap();
    outcome(session.collect(&out, &dir.join("collected"), limits, EVENT))
}

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
    entry(
        stream,
        json!({ "kind": "dir", "path": path, "mode": "0755" }),
    )
}

fn file(stream: u32, path: &str, size: u64) -> (Value, Vec<u8>) {
    entry(
        stream,
        json!({ "kind": "file", "path": path, "size": size, "mode": "0644" }),
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
    op(number, json!({ "kind": "call", "grant": "train" }), size)
}

fn finalize(number: u32, size: u64) -> (Value, Vec<u8>) {
    let call = json!({ "kind": "call", "grant": "submit", "arguments": { "finalize": "true" } });
    op(number, call, size)
}

fn op_data(number: u32, bytes: &[u8]) -> (Value, Vec<u8>) {
    (json!({ "type": "op_data", "op": number }), bytes.to_vec())
}

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

fn flight() -> Range<u32> {
    0..u32::try_from(MAX_MODEL_REQUESTS_IN_FLIGHT).unwrap()
}

// Not frames: `await_reply`, `read_frame`, `pause`, `until_hang_up`, `stall`, `stall_in_frame` and
// `hang_up_unread` mark where the fake sandbox waits, and `connect` acts on them.
fn await_reply() -> (Value, Vec<u8>) {
    (Value::Null, Vec::new())
}

fn read_frame() -> (Value, Vec<u8>) {
    (Value::from("read_frame"), Vec::new())
}

fn pause() -> (Value, Vec<u8>) {
    (Value::from("pause"), Vec::new())
}

fn until_hang_up((message, body): (Value, Vec<u8>)) -> (Value, Vec<u8>) {
    (json!({ "until_hang_up": message }), body)
}

fn stall() -> (Value, Vec<u8>) {
    (Value::from("stall"), Vec::new())
}

fn stall_in_frame() -> (Value, Vec<u8>) {
    (Value::from("stall_in_frame"), Vec::new())
}

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
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
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
                (collect, |s| {
                    // No mode a tree keeps has a set-ID or sticky bit.
                    let setuid = json!({ "kind": "file", "path": "f", "size": 0, "mode": "04755" });
                    vec![entry(s, setuid)]
                }),
                // A malformed tree ends the session, which the sandbox leaves stalling, so that the
                // gateway's end comes first; a tree that passes a limit fails only its collect.
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
            ConnectionAborted: the session failed: `04755` is not a mode: write `0` and three octal digits, such as `0644`; arrived []
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
            Some(OpPayloadLimit::NONE),
            false,
            &[
                (freeze_and_take, |s| vec![end(s), done(s)]),
                (start_and_take, |s| vec![reply("exited", s), done(s)]),
                (collect_and_take, |s| vec![dir(s, "a"), done(s), end(s)]),
                (put_after_op, |s| {
                    vec![done(s), op(0, json!({ "kind": "ops" }), 0), await_reply()]
                }),
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
            put: ConnectionAborted: the sandbox broke the protocol: it sent `done` on stream 1 out of turn: expected `failed`, or nothing before the tree's end
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
            freeze: ok; refused Call { grant: Name("train"), arguments: {} } of 5 bytes over 4: the payload is 5 bytes, more than the limit of 4: send at most 4; Call { function: Name("submit"), arguments: {Name("finalize"): "true"}, payload: [97, 98, 99, 100] }; Status { job: 3 }; ended: the sandbox ended the session; the sandbox heard op 7: {"kind":"refused","message":"the payload is 5 bytes, more than the limit of 4: send at most 4"}, op 8: {"kind":"ops","role":"agent","remaining_seconds":0,"resources":{"cpu_millis":1,"memory_bytes":1},"grants":[]}, op 9: {"kind":"ops","role":"agent","remaining_seconds":0,"resources":{"cpu_millis":1,"memory_bytes":1},"grants":[]}
            freeze: ok; ended: the sandbox broke the protocol: it sent data for op 7, which is not open
            freeze: ok; ended: the sandbox broke the protocol: it opened op 7 with a payload of 67108865 bytes, more than the protocol's 67108864
            freeze: ok; ended: the sandbox broke the protocol: it opened op 9 with a payload of 3 bytes, for a call that takes none
            freeze: ok; ended: the sandbox broke the protocol: it sent 3 bytes of op 7's payload, past its size: 2 were left
            freeze: ok; ended: the sandbox broke the protocol: it opened op 8 while op 7 is in flight
            freeze: ConnectionAborted: the sandbox broke the protocol: it sent data for op 7, which is not open; Call { function: Name("train"), arguments: {}, payload: [97] }; reply: ConnectionAborted: the sandbox broke the protocol: it sent data for op 7, which is not open; ended: the sandbox broke the protocol: it sent data for op 7, which is not open
            freeze: ConnectionAborted: the sandbox broke the protocol: it opened op 8 while op 7 is in flight; Call { function: Name("train"), arguments: {}, payload: [] }; reply: ConnectionAborted: the sandbox broke the protocol: it opened op 8 while op 7 is in flight; ended: the sandbox broke the protocol: it opened op 8 while op 7 is in flight
            freeze: ConnectionAborted: the sandbox broke the protocol: it sent data for op 99, which is not open; refused Call { grant: Name("train"), arguments: {} } of 5 bytes over 4: the payload is 5 bytes, more than the limit of 4: send at most 4; refuse: ConnectionAborted: the sandbox broke the protocol: it sent data for op 99, which is not open; ended: the sandbox broke the protocol: it sent data for op 99, which is not open
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
                // The sandbox stalls inside the response's first frame of data.
                (freeze_and_take, |s| {
                    let stale = model_request(3, "/stale", 0);
                    vec![done(s), stale, read_frame(), stall_in_frame()]
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
            r#"the sessions' commands equal: false; op 0 on another session: panicked: internal error: entered unreachable code: op 0 was answered on a session other than the one it arrived on, though the contracts of `reply` and `refuse` require the one that handed it out; that session's own op 0: ok; its op 1 refused on another session: panicked: internal error: entered unreachable code: op 1 was answered on a session other than the one it arrived on, though the contracts of `reply` and `refuse` require the one that handed it out; the sandbox heard op 0: {"kind":"ops","role":"agent","remaining_seconds":0,"resources":{"cpu_millis":1,"memory_bytes":1},"grants":[]}"#
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
                // The sandbox pauses, or stops reading a large put, then reads on: the requests
                // wait, as their deadlines haven't passed. It answers the put after reading its
                // entry but before reading the entry's data, and the gateway takes the answer once
                // the put is all written.
                (
                    |session, _, _| freezes(session, hung()),
                    |s| vec![pause(), done(s)],
                ),
                (
                    |session, _, _| outcome(put_large(session, hung())),
                    |s| vec![read_frame(), pause(), done(s), await_reply()],
                ),
                // The sandbox answers nothing, stops reading inside a large put's first frame of
                // data, after its entry's, or sends a tree that never ends, until the gateway hangs
                // up. Each request's deadline is the sandbox's stall.
                (|session, _, _| freezes(session, EVENT), |_| vec![stall()]),
                (
                    |session, _, _| {
                        let put = put_large(session, EVENT);
                        then_freeze(session, put)
                    },
                    |_| vec![read_frame(), stall_in_frame()],
                ),
                (collect_until_stalled, |s| {
                    vec![file(s, "f", u64::MAX), until_hang_up(data(s, b"x"))]
                }),
                // In frames of a full body, which the request takes longer to write than the reader
                // takes to read, so that the reader waits on the request's queue as the deadline
                // passes, and must not go on waiting there once the request has failed.
                (collect_until_stalled, |s| {
                    let body = vec![b'x'; frame::MAX_BODY_BYTES];
                    vec![file(s, "f", u64::MAX), until_hang_up(data(s, &body))]
                }),
                // A deadline that has passed before a request or a reply starts, which sends
                // nothing.
                (
                    |session, _, _| freezes(session, Duration::ZERO),
                    |_| Vec::new(),
                ),
                (late_reply, |s| {
                    vec![done(s), op(0, json!({ "kind": "ops" }), 0), await_reply()]
                }),
                // An answer that came before the deadline passed is taken, though its request
                // looks only after.
                (start_late, |s| {
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
            start: ok; reply: ok; the sandbox heard op 0: {"kind":"ops","role":"agent","remaining_seconds":0,"resources":{"cpu_millis":1,"memory_bytes":1},"grants":[]}
            ok; then ConnectionAborted: the sandbox ended the session; then by a passed deadline ConnectionAborted: the sandbox ended the session
            ConnectionAborted: the sandbox ended the session; arrived ["a/"]"#]],
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
                let accepted =
                    registration.accept((&Machine, by), None, false, sender, |a| a, stopped);
                let returned = time::left(&Machine, by).map_or("past", |_| "before");
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
            let cases: [(&str, Opening, Duration); 14] = [
                (
                    "this version",
                    |c| frame::write(c, &hello(), &[]).unwrap(),
                    hung(),
                ),
                (
                    "a v0 sandboxd",
                    |c| frame::write(c, &json!({ "version": 0 }), &[]).unwrap(),
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
                    "this version, with an attestation",
                    |c| frame::write(c, &hello(), b"x").unwrap(),
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
                        frame::write(&mut written, &hello(), &[]).unwrap();
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
            a v0 sandboxd: heard {"message":"sandboxd speaks sandbox protocol 0: expected <version>, which this gateway speaks, as a sandboxd built from the same Pumpkin does","type":"refused"}; InvalidData: the gateway refused the session: sandboxd speaks sandbox protocol 0: expected <version>, which this gateway speaks, as a sandboxd built from the same Pumpkin does
            a later version, with a field this one lacks: heard {"message":"sandboxd speaks sandbox protocol <later version>: expected <version>, which this gateway speaks, as a sandboxd built from the same Pumpkin does","type":"refused"}; InvalidData: the gateway refused the session: sandboxd speaks sandbox protocol <later version>: expected <version>, which this gateway speaks, as a sandboxd built from the same Pumpkin does
            this version, with a field it lacks: heard {"message":"the hello isn't of sandbox protocol <version>'s form: unknown field `later`, expected `version` or `gpus`","type":"refused"}; InvalidData: the gateway refused the session: the hello isn't of sandbox protocol <version>'s form: unknown field `later`, expected `version` or `gpus`
            a later version, as long as a hello may be: heard {"message":"sandboxd speaks sandbox protocol <later version>: expected <version>, which this gateway speaks, as a sandboxd built from the same Pumpkin does","type":"refused"}; InvalidData: the gateway refused the session: sandboxd speaks sandbox protocol <later version>: expected <version>, which this gateway speaks, as a sandboxd built from the same Pumpkin does
            a length a byte over a hello's, and nothing more: heard {"message":"the hello is 1025 bytes: a hello holds at most 1024","type":"refused"}; InvalidData: the gateway refused the session: the hello is 1025 bytes: a hello holds at most 1024
            this version, with an attestation: heard {"message":"the hello presents an attestation on a socket of the sandbox's own, which identifies it: expected none there","type":"refused"}; InvalidData: the gateway refused the session: the hello presents an attestation on a socket of the sandbox's own, which identifies it: expected none there
            a reply, not a hello: heard {"message":"the session's first frame is no hello: missing field `version`: open the session with one","type":"refused"}; InvalidData: the gateway refused the session: the session's first frame is no hello: missing field `version`: open the session with one
            a frame whose message, `null`, is no object: heard {"message":"the session's first frame is no hello: invalid type: null, expected an object with a `version`: open the session with one","type":"refused"}; InvalidData: the gateway refused the session: the session's first frame is no hello: invalid type: null, expected an object with a `version`: open the session with one
            a frame whose message, `hello`, isn't JSON: heard {"message":"a frame's message isn't JSON: expected value at line 1 column 1","type":"refused"}; InvalidData: the gateway refused the session: a frame's message isn't JSON: expected value at line 1 column 1
            part of a frame, then the connection's end: heard nothing; UnexpectedEof: the sandbox closed the connection inside its first frame: expected a whole hello
            a hello's message, then the connection's end: heard nothing; UnexpectedEof: the sandbox closed the connection inside its first frame: expected a whole hello
            nothing, then the connection's end: heard nothing; UnexpectedEof: the sandbox closed the connection before its hello: expected it to open its session with one
            nothing, past the deadline: heard nothing; TimedOut: the sandbox connected but sent no hello by the deadline: expected its sandboxd to send one as it connects, as a sandboxd built from the same Pumpkin does"#]],
    );
}

fn listen(hello_timeout: Duration) -> (Listener, SocketAddr) {
    let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let address = GatewayAddress::try_from(loopback).unwrap();
    let listener = Listener::bind(&address, hello_timeout).unwrap();
    let (PrivateDial { gateway }, _) = listener.issue(Deadline::after(HUNG));
    (listener, gateway)
}

fn dial(address: SocketAddr) -> TcpStream {
    let connection = TcpStream::connect(address).unwrap();
    connection.set_read_timeout(Some(HUNG)).unwrap();
    connection
}

fn answered(connection: &mut TcpStream) -> String {
    match frame::read::<Value>(connection) {
        Ok(Some((answer, _))) => answer.to_string(),
        Ok(None) => "none".to_owned(),
        Err(error) => format!("{:?}: {error}", error.kind()),
    }
}

type Said = (Value, Vec<u8>);

fn attesting(attestation: &str) -> Said {
    let hello = serde_json::to_value(hello()).unwrap();
    (hello, attestation.as_bytes().to_vec())
}

fn present(address: SocketAddr, (hello, body): &Said) -> String {
    let mut connection = dial(address);
    frame::write(&mut connection, hello, body).unwrap();
    answered(&mut connection)
}

fn issue(listener: &Listener, address: SocketAddr, by: Deadline) -> Unattested {
    let (PrivateDial { gateway }, unattested) = listener.issue(by);
    assert_eq!(gateway, address, "the address the dial names");
    unattested
}

/// The registration whose check admits `attestation` from `from` alone, and says which it
/// refuses for: another attestation, or another address.
fn admitting(unattested: Unattested, attestation: &'static str, from: Ipv4Addr) -> Registration {
    unattested.admitting(move |presented, peer| {
        if presented.bytes() != attestation.as_bytes() {
            return Err("it attests another sandbox".to_owned());
        }
        if peer != IpAddr::V4(from) {
            return Err(format!("it comes from {peer}, where its sandbox isn't"));
        }
        Ok(())
    })
}

/// A registration whose check admits `attestation` from the loopback, where each hello comes from.
fn admitted(listener: &Listener, address: SocketAddr, attestation: &'static str) -> Registration {
    admitting(
        issue(listener, address, Deadline::after(HUNG)),
        attestation,
        Ipv4Addr::LOCALHOST,
    )
}

fn hello() -> Hello {
    Hello { gpus: None }
}

type Named = fn(stream: u32, name: &str) -> Vec<Value>;

fn named(stream: u32, name: &str) -> Vec<Value> {
    vec![json!({ "type": "failed", "stream": stream, "message": format!("I am {name}") })]
}

fn sandbox(
    address: SocketAddr,
    (hello, body): Said,
    pause: Duration,
    answer: Named,
    said: (Sender<String>, &'static str),
) -> JoinHandle<()> {
    thread::spawn(move || {
        thread::sleep(pause);
        let mut connection = dial(address);
        frame::write(&mut connection, &hello, &body).unwrap();
        let opened = answered(&mut connection);
        let (said, name) = said;
        said.send(format!("{name}: {opened}")).unwrap();
        if opened != r#"{"type":"accepted"}"# {
            return;
        }
        let (request, _) = frame::read::<ToSandbox>(&mut connection).unwrap().unwrap();
        for message in answer(stream_of(&request).unwrap(), name) {
            frame::write(&mut connection, &message, &[]).unwrap();
        }
        while let Ok(Some(_)) = frame::read::<ToSandbox>(&mut connection) {}
    })
}

fn twice(address: SocketAddr, said: &Sender<String>, name: &'static str) -> [JoinHandle<()>; 2] {
    [(); 2].map(|()| {
        let said = (said.clone(), name);
        sandbox(address, attesting(name), Duration::ZERO, named, said)
    })
}

fn accept_and_freeze(registration: Registration, by: Duration) -> String {
    let (sender, _arrivals) = mpsc::channel();
    let accepted = registration.accept((&Machine, by), None, false, sender, |a| a, || false);
    outcome(accepted.and_then(|mut session| session.freeze(hung())))
}

fn refusals(listener: &Listener, address: SocketAddr) -> Vec<String> {
    let late = admitted(listener, address, "late");
    let waited = accept_and_freeze(late, Duration::ZERO);
    drop(admitted(listener, address, "dropped"));
    let unspent = issue(listener, address, Deadline::after(HUNG));
    let elsewhere = admitting(unspent, "elsewhere", Ipv4Addr::new(192, 0, 2, 1));

    // Its check never comes, and no hello waits for it, as its deadline has passed.
    let _expired = issue(listener, address, Deadline::after(Duration::ZERO));
    let version = |version| json!({ "version": version });
    let hellos = [
        ("no attestation", (version(PROTOCOL_VERSION), Vec::new())),
        (
            "a later version",
            (version(PROTOCOL_VERSION + 1), b"late".to_vec()),
        ),
        (
            "a field this version lacks",
            (
                json!({ "version": PROTOCOL_VERSION, "later": true }),
                b"late".to_vec(),
            ),
        ),
        ("an attestation that no check admits", attesting("foreign")),
        (
            "an attestation from where its check admits none",
            attesting("elsewhere"),
        ),
    ];
    let mut lines: Vec<String> = (hellos.iter())
        .map(|(name, hello)| format!("{name}: {}", present(address, hello)))
        .collect();
    // Its accept tells why its check refused the last hello that no check admitted.
    let elsewhere = accept_and_freeze(elsewhere, Duration::ZERO);
    lines.extend([
        format!("an accept by a passed deadline: {waited}"),
        format!("  of another, once its check refused a hello: {elsewhere}"),
        format!(
            "  its attestation: {}",
            present(address, &attesting("late"))
        ),
        format!(
            "an attestation whose registration dropped: {}",
            present(address, &attesting("dropped"))
        ),
    ]);
    lines
}

fn awaited(listener: &Listener, address: SocketAddr) -> Vec<String> {
    let (said, heard) = mpsc::channel();
    let registration = admitted(listener, address, "awaited");
    let answer: Named = |s, _| vec![done(s).0, op(0, json!({ "kind": "ops" }), 0).0];
    let hello = attesting("awaited");
    let sandbox = sandbox(address, hello.clone(), PAUSE, answer, (said, "awaited"));
    let ops = Some(OpPayloadLimit::NONE);
    let (sender, arrivals) = mpsc::channel();
    let accepted = registration.accept((&Machine, hung()), ops, false, sender, |a| a, || false);
    let mut session = accepted.unwrap();
    let frozen = outcome(session.freeze(hung()));
    let mut lines = vec![format!("{}; freeze: {frozen}", heard.recv().unwrap())];
    let op = next_op(&arrivals);
    lines.push(format!("  its op: {:?}", op.op));
    session.reply(op, no_grants(), hung()).unwrap();
    drop(session);
    sandbox.join().unwrap();
    lines.push(format!(
        "  its attestation, again: {}",
        present(address, &hello)
    ));
    lines
}

fn early(listener: &Listener, address: SocketAddr) -> Vec<String> {
    let (said, heard) = mpsc::channel();
    let pairs = ["a", "b"].map(|name| {
        let registration = admitted(listener, address, name);
        (name, registration, twice(address, &said, name))
    });
    let mut refused: Vec<String> = (0..2).map(|_| heard.recv().unwrap()).collect();
    refused.sort();
    let mut lines = vec![format!("presented early: {}", refused.join("; "))];
    for (name, registration, sandboxes) in pairs.into_iter().rev() {
        let accepted = accept_and_freeze(registration, hung());
        lines.push(format!("  {name} accepted, then a freeze: {accepted}"));
        for sandbox in sandboxes {
            sandbox.join().unwrap();
        }
    }
    lines
}

fn unchecked(listener: &Listener, address: SocketAddr) -> Vec<String> {
    let (said, heard) = mpsc::channel();
    let [checked, withdrawn] = [(); 2].map(|()| issue(listener, address, Deadline::after(HUNG)));
    let hellos = ["checked", "withdrawn"].map(|name| {
        let said = (said.clone(), name);
        sandbox(address, attesting(name), Duration::ZERO, named, said)
    });
    // The hellos wait for the checks, which come once they have been read, as a create's do, or
    // never: no outcome rests on which comes first.
    thread::sleep(PAUSE);
    let registration = admitting(checked, "checked", Ipv4Addr::LOCALHOST);
    drop(withdrawn);
    let accepted = accept_and_freeze(registration, hung());
    let mut told: Vec<String> = (0..2).map(|_| heard.recv().unwrap()).collect();
    told.sort();
    for sandbox in hellos {
        sandbox.join().unwrap();
    }
    vec![
        format!("presented before the checks: {}", told.join("; ")),
        format!("  checked accepted, then a freeze: {accepted}"),
    ]
}

fn dropped(listener: &Listener, address: SocketAddr) -> Vec<String> {
    let (said, heard) = mpsc::channel();
    let registration = admitted(listener, address, "c");
    let sandboxes = twice(address, &said, "c");
    let refused = heard.recv().unwrap();
    drop(registration);
    let closed = heard.recv().unwrap();
    for sandbox in sandboxes {
        sandbox.join().unwrap();
    }
    vec![
        format!("a registration dropped once presented: {refused}; then {closed}"),
        format!("  its attestation: {}", present(address, &attesting("c"))),
    ]
}

fn stopped_accept(listener: &Listener, address: SocketAddr) -> Vec<String> {
    let registration = admitted(listener, address, "stopped");
    let by = hung();
    let looks = Cell::new(0);
    let stopped = || {
        looks.set(looks.get() + 1);
        looks.get() > 2
    };
    let (sender, _) = mpsc::channel();
    let accepted = registration.accept((&Machine, by), None, false, sender, |a| a, stopped);
    let accepted = outcome(accepted.map(drop));
    let returned = time::left(&Machine, by).map_or("past", |_| "before");
    let presented = present(address, &attesting("stopped"));
    vec![
        format!("an accept stopped as it waits returned {returned} its deadline: {accepted}"),
        format!("  its attestation: {presented}"),
    ]
}

fn meet(address: SocketAddr, attestation: &str, waiting: JoinHandle<String>) -> String {
    let (said, heard) = mpsc::channel();
    let hello = attesting(attestation);
    let sandbox = sandbox(address, hello, Duration::ZERO, named, (said, "meanwhile"));
    let accepted = waiting.join().unwrap();
    sandbox.join().unwrap();
    let heard = heard.recv().unwrap();
    format!("the accept waiting meanwhile: {heard}; then a freeze: {accepted}")
}

fn crowd() -> String {
    let (_listener, address) = listen(HELLO_TIMEOUT);
    let silent: Vec<TcpStream> = (0..HELLOS_AT_ONCE).map(|_| dial(address)).collect();
    let heard = answered(&mut dial(address));
    drop(silent);
    format!("one past the hellos read at once: heard {heard}")
}

fn turns() -> String {
    let (_listener, address) = listen(HELLO_TIMEOUT);
    let hello = (json!({ "version": PROTOCOL_VERSION }), Vec::new());
    let hellos = HELLOS_AT_ONCE + 1;
    let mut heard: Vec<String> = (0..hellos).map(|_| present(address, &hello)).collect();
    heard.dedup();
    format!("{hellos} hellos, each in turn: {}", heard.join("; "))
}

#[test]
fn gateway_addresses() {
    check(
        |_| {
            let addresses = [
                "127.0.0.1:0",
                "10.0.0.7:7433",
                "[::1]:7433",
                "0.0.0.0:7433",
                "[::]:7433",
                "[::ffff:0.0.0.0]:7433",
            ];
            (addresses.iter())
                .map(|address| {
                    let parsed = GatewayAddress::try_from(address.parse::<SocketAddr>().unwrap());
                    let parsed = parsed.map_or_else(|error| error, |_| "taken".to_owned());
                    format!("{address}: {parsed}")
                })
                .collect()
        },
        &expect![[r#"
            127.0.0.1:0: taken
            10.0.0.7:7433: taken
            [::1]:7433: taken
            0.0.0.0:7433: the gateway's address 0.0.0.0:7433 is unspecified, which no sandbox can dial: name the address its sandboxes dial
            [::]:7433: the gateway's address [::]:7433 is unspecified, which no sandbox can dial: name the address its sandboxes dial
            [::ffff:0.0.0.0]:7433: the gateway's address [::ffff:0.0.0.0]:7433 is unspecified, which no sandbox can dial: name the address its sandboxes dial"#]],
    );
}

#[test]
fn registrations() {
    check(
        |_| {
            let (listener, address) = listen(HELLO_TIMEOUT);
            let (_bounded, bounded_address) = listen(HELLO_BOUND);
            let mut silent = dial(bounded_address);
            let meanwhile = admitted(&listener, address, "meanwhile");
            let waiting = thread::spawn(|| accept_and_freeze(meanwhile, hung()));
            let mut lines = refusals(&listener, address);
            for meeting in [awaited, early, unchecked, dropped, stopped_accept] {
                lines.extend(meeting(&listener, address));
            }
            lines.push(meet(address, "meanwhile", waiting));
            let untold = answered(&mut silent);
            lines.push(format!("the peer silent from the first: heard {untold}"));
            drop(listener);
            let refused = TcpStream::connect(address).map(drop).map_err(|e| e.kind());
            lines.push(format!("a dial once the listener dropped: {refused:?}"));
            lines.extend([crowd(), turns()]);
            lines
        },
        &expect![[r#"
            no attestation: {"message":"the hello presents no attestation: expected the sandbox's, at an address that several sandboxes share","type":"refused"}
            a later version: {"message":"sandboxd speaks sandbox protocol <later version>: expected <version>, which this gateway speaks, as a sandboxd built from the same Pumpkin does","type":"refused"}
            a field this version lacks: {"message":"the hello isn't of sandbox protocol <version>'s form: unknown field `later`, expected `version` or `gpus`","type":"refused"}
            an attestation that no check admits: {"message":"the attestation opens no session: it attests another sandbox: expected the attestation of a sandbox whose start waits, presented once","type":"refused"}
            an attestation from where its check admits none: {"message":"the attestation opens no session: it attests another sandbox; it comes from 127.0.0.1, where its sandbox isn't: expected the attestation of a sandbox whose start waits, presented once","type":"refused"}
            an accept by a passed deadline: TimedOut: no hello that the sandbox's check admits came by the deadline: expected the sandbox to say it while the deadline lasts
              of another, once its check refused a hello: TimedOut: no hello that the sandbox's check admits came by the deadline, and it refused the last hello that none admitted: it comes from 127.0.0.1, where its sandbox isn't: expected the sandbox to say it while the deadline lasts
              its attestation: {"message":"the attestation opens no session: it attests another sandbox: expected the attestation of a sandbox whose start waits, presented once","type":"refused"}
            an attestation whose registration dropped: {"message":"the attestation opens no session: it attests another sandbox: expected the attestation of a sandbox whose start waits, presented once","type":"refused"}
            awaited: {"type":"accepted"}; freeze: ok
              its op: Ops
              its attestation, again: {"message":"the attestation opens no session: it attests another sandbox: expected the attestation of a sandbox whose start waits, presented once","type":"refused"}
            presented early: a: {"message":"the attestation was presented already, on another connection: expected it once, as it opens one session","type":"refused"}; b: {"message":"the attestation was presented already, on another connection: expected it once, as it opens one session","type":"refused"}
              b accepted, then a freeze: Other: I am b
              a accepted, then a freeze: Other: I am a
            presented before the checks: checked: {"type":"accepted"}; withdrawn: {"message":"the attestation opens no session: it attests another sandbox: expected the attestation of a sandbox whose start waits, presented once","type":"refused"}
              checked accepted, then a freeze: Other: I am checked
            a registration dropped once presented: c: {"message":"the attestation was presented already, on another connection: expected it once, as it opens one session","type":"refused"}; then c: none
              its attestation: {"message":"the attestation opens no session: it attests another sandbox: expected the attestation of a sandbox whose start waits, presented once","type":"refused"}
            an accept stopped as it waits returned before its deadline: Other: the accept was stopped before the sandbox's hello was admitted: expected the sandbox to say it before the stop
              its attestation: {"message":"the attestation opens no session: it attests another sandbox: expected the attestation of a sandbox whose start waits, presented once","type":"refused"}
            the accept waiting meanwhile: meanwhile: {"type":"accepted"}; then a freeze: Other: I am meanwhile
            the peer silent from the first: heard none
            a dial once the listener dropped: Err(ConnectionRefused)
            one past the hellos read at once: heard none
            101 hellos, each in turn: {"message":"the hello presents no attestation: expected the sandbox's, at an address that several sandboxes share","type":"refused"}"#]],
    );
}

#[test]
fn dials() {
    check(
        |_| {
            let dialer = Dialer::new(test_support::edge::client());
            let issue = || {
                let (Listening { grant }, unreached) = dialer.issue().unwrap();
                (grant, unreached)
            };
            let accepted = |registration: Registration, by, stopped: bool| {
                let (sender, _) = mpsc::channel();
                let session =
                    registration.accept((&Machine, by), None, false, sender, |a| a, || stopped);
                session
                    .map(drop)
                    .map_err(|error| format!("{:?}: {error}", error.kind()))
            };
            let (grant, unreached) = issue();
            let (port, sandbox) = listening(true, (json!(hello()), Vec::new()));
            let session = accepted(unreached.at(endpoint(port)), hung(), false);
            let heard = sandbox.join().unwrap();
            let presented = heard == Some(HelloReply::Accepted { grant: Some(grant) });
            let mut lines = vec![format!(
                "a sandbox that listens once a dial has failed: {session:?}; it heard its grant: \
                 {presented}"
            )];
            let attested = attesting("an attestation");
            let later = (json!({ "version": PROTOCOL_VERSION + 1 }), Vec::new());
            for (case, hello) in [("with an attestation", attested), ("later", later)] {
                let (_, unreached) = issue();
                let (port, sandbox) = listening(false, hello);
                let session = accepted(unreached.at(endpoint(port)), hung(), false);
                let heard = sandbox.join().unwrap();
                lines.push(format!("a hello {case}: {session:?}; it heard {heard:?}"));
            }
            let closed = TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let (_, unreached) = issue();
            // Its deadline passes once the first dial has failed.
            let (sender, _) = mpsc::channel();
            let registration = unreached.at(endpoint(closed));
            let clock = Still::after_a_read();
            let session =
                registration.accept((&clock, EVENT), None, false, sender, |a| a, || false);
            let session = session
                .map(drop)
                .map_err(|error| format!("{:?}: {error}", error.kind()));
            let session = session.map_err(|error| error.replace(&closed.to_string(), "$PORT"));
            lines.push(format!("nothing listening: {session:?}"));
            lines.push(format!(
                "a certificate the dialer doesn't trust: {}",
                untrusted()
            ));
            let (_, unreached) = issue();
            let session = accepted(unreached.at(endpoint(closed)), hung(), true);
            let session = session.map_err(|error| error.replace(&closed.to_string(), "$PORT"));
            lines.push(format!("stopped: {session:?}"));
            lines
        },
        &expect![[r#"
            a sandbox that listens once a dial has failed: Ok(()); it heard its grant: true
            a hello with an attestation: Err("InvalidData: the gateway refused the session: the hello presents an attestation where the gateway dialed the sandbox, which its endpoint identifies: expected none there"); it heard Some(Refused { message: "the hello presents an attestation where the gateway dialed the sandbox, which its endpoint identifies: expected none there" })
            a hello later: Err("InvalidData: the gateway refused the session: sandboxd speaks sandbox protocol <later version>: expected <version>, which this gateway speaks, as a sandboxd built from the same Pumpkin does"); it heard Some(Refused { message: "sandboxd speaks sandbox protocol <later version>: expected <version>, which this gateway speaks, as a sandboxd built from the same Pumpkin does" })
            nothing listening: Err("TimedOut: the gateway didn't reach the sandbox at localhost:$PORT by the deadline: its last dial failed: dialing localhost:$PORT failed: Connection refused (os error 111): expected its sandboxd to listen there while the deadline lasts")
            a certificate the dialer doesn't trust: Err("InvalidData: the gateway can't trust the sandbox's endpoint localhost:$PORT: the TLS handshake failed: invalid peer certificate: UnknownIssuer: expected TLS 1.3 from `localhost`, with a certificate trusted for it"); Err(InvalidData)
            stopped: Err("Other: the accept was stopped before the gateway reached the sandbox at localhost:$PORT: expected its sandboxd to listen there before the stop")"#]],
    );
}

fn untrusted() -> String {
    let (_, unreached) = Dialer::new(tls::Client::web_pki()).issue().unwrap();
    let edge = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = edge.local_addr().unwrap().port();
    let shown = thread::spawn(move || {
        let (stream, _) = edge.accept().unwrap();
        let shown = test_support::edge::server().accept(stream, Deadline::after(HUNG));
        shown.map(drop).map_err(|error| error.kind())
    });
    let (sender, _) = mpsc::channel();
    let registration = unreached.at(endpoint(port));
    let session = registration.accept((&Machine, hung()), None, false, sender, |a| a, || false);
    let session = session.map(drop).map_err(|error| {
        format!("{:?}: {error}", error.kind()).replace(&port.to_string(), "$PORT")
    });
    format!("{session:?}; {:?}", shown.join().unwrap())
}

fn endpoint(port: u16) -> Endpoint {
    Endpoint::new(test_support::edge::NAME.to_owned(), port).unwrap()
}

fn listening(closes_first: bool, (hello, body): Said) -> (u16, JoinHandle<Option<HelloReply>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let sandbox = thread::spawn(move || {
        if closes_first {
            drop(listener.accept().unwrap());
        }
        let (stream, _) = listener.accept().unwrap();
        let stream = test_support::edge::server()
            .accept(stream, Deadline::after(HUNG))
            .unwrap();
        frame::write(&mut &stream, &hello, &body).unwrap();
        frame::read::<HelloReply>(&mut &stream)
            .unwrap()
            .map(|(answer, _)| answer)
    });
    (port, sandbox)
}

// The deadline of an accept whose peer stalls, on a `Still` whose event is the stall, and so the
// bound of each of its dials. A dial that ends before the stall is made again.
const STALL_BOUND: Duration = Duration::from_millis(250);

// What a fake companion says once a connection has its TLS: a hello, or nothing, having marked
// that it has come so far.
enum Says {
    Hello(Said),
    Nothing(Arc<AtomicBool>),
}

fn companion_named(name: &str) -> CompanionName {
    CompanionName::try_from(name.to_owned()).unwrap()
}

fn relayed_to(dialer: &Dialer, name: &str) -> (RegistrationGrant, TlsIdentity, Unreached) {
    let (Relayed { grant, identity }, unreached) = dialer.relayed(&companion_named(name)).unwrap();
    (grant, identity, unreached)
}

/// A fake companion named `name`, which serves `server`'s TLS to each connection, then says
/// `says`, and sends what the gateway answered. Accepted, it answers as [`named`].
fn companion(
    server: tls::Server,
    name: &'static str,
    says: Says,
) -> (SocketAddr, Receiver<Option<HelloReply>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (heard, answers) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = server.accept(stream.unwrap(), Deadline::after(HUNG)) else {
                continue;
            };
            let (hello, body) = match &says {
                Says::Hello(said) => said,
                Says::Nothing(reached) => {
                    reached.store(true, Ordering::Relaxed);
                    // Silent until the gateway ends the connection.
                    let _ = (&stream).read(&mut [0]);
                    continue;
                }
            };
            frame::write(&mut &stream, hello, body).unwrap();
            let answer = frame::read::<HelloReply>(&mut &stream).ok().flatten();
            let answer = answer.map(|(answer, _)| answer);
            let accepted = matches!(answer, Some(HelloReply::Accepted { .. }));
            // Nothing receives it once the case has its outcome.
            let _ = heard.send(answer);
            if !accepted {
                continue;
            }
            let Ok(Some((request, _))) = frame::read::<ToSandbox>(&mut &stream) else {
                continue;
            };
            for message in named(stream_of(&request).unwrap(), name) {
                frame::write(&mut &stream, &message, &[]).unwrap();
            }
            while let Ok(Some(_)) = frame::read::<ToSandbox>(&mut &stream) {}
        }
    });
    (address, answers)
}

/// Whether `carried` holds none of the bytes of a session whose grant is `grant`. Those are the
/// grant's digits, and what each frame of the session's holds.
fn unread(carried: &[&[u8]], grant: &RegistrationGrant) -> bool {
    let session = [&grant.digits(), "\"type\"", "\"version\"", "I am"];
    let holds = |needle: &str| {
        (carried.iter()).any(|carried| {
            carried
                .windows(needle.len())
                .any(|at| at == needle.as_bytes())
        })
    };
    !session.into_iter().any(holds)
}

/// The outcome of the accept of `unreached` at the relay behind the edge's `port`.
fn accepted<C: Clock>(
    unreached: Unreached,
    port: u16,
    (clock, by): (&C, Duration),
    stopped: bool,
) -> String {
    let (sender, _) = mpsc::channel();
    let registration = unreached.at(endpoint(port));
    let session = registration.accept((clock, by), None, false, sender, |a| a, || stopped);
    let session = (session.map(drop)).map_err(|error| format!("{:?}: {error}", error.kind()));
    format!("{session:?}").replace(&port.to_string(), "$PORT")
}

#[test]
fn relayed_meetings() {
    check(
        |_| {
            let dialer = Dialer::new(edge::client());
            let met = ["scheduler", "swe-000"].map(|name| {
                let (grant, identity, unreached) = relayed_to(&dialer, name);
                let said = Says::Hello((json!(hello()), Vec::new()));
                let (address, heard) = companion(relayed::server(&identity), name, said);
                (name, grant, unreached, address, heard)
            });
            let behind =
                (met.iter()).map(|(name, .., address, _)| (companion_named(name), *address));
            let relay = relayed::relay(behind.collect());
            let port = edge::front(relay.address);
            let lines = met.map(|(name, grant, unreached, _, heard)| {
                let session = accept_and_freeze(unreached.at(endpoint(port)), hung());
                let Carried {
                    to_companion,
                    to_gateway,
                } = relay.ended();
                // Each way begins with a record of TLS's handshake, so the relay did carry it.
                let tls = [&to_companion, &to_gateway].map(|sent| sent.starts_with(&[0x16, 0x03]));
                let unread = unread(&[&to_companion, &to_gateway], &grant);
                let granted = HelloReply::Accepted { grant: Some(grant) };
                let presented = heard.recv_timeout(HUNG).unwrap() == Some(granted);
                format!(
                    "companion `{name}`, through its sandbox's relay: {session}; it heard its \
                     grant: {presented}\n  the relay carried TLS each way: {tls:?}, and none of \
                     the session's bytes: {unread}"
                )
            });
            lines.to_vec()
        },
        &expect![[r#"
            companion `scheduler`, through its sandbox's relay: Other: I am scheduler; it heard its grant: true
              the relay carried TLS each way: [true, true], and none of the session's bytes: true
            companion `swe-000`, through its sandbox's relay: Other: I am swe-000; it heard its grant: true
              the relay carried TLS each way: [true, true], and none of the session's bytes: true"#]],
    );
}

/// A relay behind an edge, which carries to one fake companion, named `name`. That one serves
/// `server`'s TLS and says `says`. Returns the edge's port and the relay.
fn relayed_behind(name: &'static str, server: tls::Server, says: Says) -> (u16, relayed::Relay) {
    let (address, _) = companion(server, name, says);
    let relay = relayed::relay(BTreeMap::from([(companion_named(name), address)]));
    (edge::front(relay.address), relay)
}

/// Waits for a connection that `relay` carried to end, and says whether it is [`unread`].
fn ended_unread(relay: &relayed::Relay, grant: &RegistrationGrant) -> bool {
    let Carried {
        to_companion,
        to_gateway,
    } = relay.ended();
    unread(&[&to_companion, &to_gateway], grant)
}

#[test]
fn untrusted_companions() {
    check(
        |_| {
            let dialer = Dialer::new(edge::client());
            let said = || Says::Hello((json!(hello()), Vec::new()));
            // Each accept dials once, as a companion that it can't trust fails it for good.
            let refused = |name: &'static str, server: tls::Server| {
                let (grant, _, unreached) = relayed_to(&dialer, name);
                let (port, relay) = relayed_behind(name, server, said());
                let outcome = accepted(unreached, port, (&Machine, hung()), false);
                assert!(ended_unread(&relay, &grant));
                outcome
            };
            let (_, another, _) = relayed_to(&dialer, "swe-000");
            let foreign = refused("scheduler", relayed::server(&another));
            let (_, again, _) = relayed_to(&dialer, "scheduler");
            let rekeyed = refused("scheduler", relayed::server(&again));
            let public = refused(edge::OTHER_NAME, edge::other_server());
            // The relay says a hello of its own, in the clear, and hears what the gateway sends.
            let (grant, _, unreached) = relayed_to(&dialer, "scheduler");
            let (heard, answers) = mpsc::channel();
            let port = edge::front_of(move |mut stream| {
                read_route(&mut stream).unwrap();
                frame::write(&mut stream, &json!(hello()), &[]).unwrap();
                let mut answered = Vec::new();
                let _ = stream.read_to_end(&mut answered);
                heard.send(answered).unwrap();
            });
            let answering = accepted(unreached, port, (&Machine, hung()), false);
            assert!(unread(&[&answers.recv_timeout(HUNG).unwrap()], &grant));
            vec![
                format!("the certificate issued for another companion: {foreign}"),
                format!("one issued for its name later, of another key: {rekeyed}"),
                format!("one for its name that the dialer's own roots vouch for: {public}"),
                format!("a relay that answers for the companion: {answering}"),
            ]
        },
        &expect![[r#"
            the certificate issued for another companion: Err("InvalidData: the gateway can't trust companion `scheduler` behind the relay at localhost:$PORT: the TLS handshake failed: invalid peer certificate: UnknownIssuer: expected TLS 1.3 from `scheduler`, with a certificate trusted for it")
            one issued for its name later, of another key: Err("InvalidData: the gateway can't trust companion `scheduler` behind the relay at localhost:$PORT: the TLS handshake failed: invalid peer certificate: BadSignature: expected TLS 1.3 from `scheduler`, with a certificate trusted for it")
            one for its name that the dialer's own roots vouch for: Err("InvalidData: the gateway can't trust companion `companion` behind the relay at localhost:$PORT: the TLS handshake failed: invalid peer certificate: UnknownIssuer: expected TLS 1.3 from `companion`, with a certificate trusted for it")
            a relay that answers for the companion: Err("InvalidData: the gateway can't trust companion `scheduler` behind the relay at localhost:$PORT: the TLS handshake failed: received corrupt message of type InvalidContentType: expected TLS 1.3 from `scheduler`, with a certificate trusted for it")"#]],
    );
}

#[test]
fn closing_relays() {
    check(
        |_| {
            let dialer = Dialer::new(edge::client());
            // An accept whose deadline passes once its first dial has failed.
            let once = |port: u16| {
                let (_, _, unreached) = relayed_to(&dialer, "scheduler");
                accepted(unreached, port, (&Still::after_a_read(), EVENT), false)
            };
            let unrouted = once(edge::front_of(drop));
            let refused = once(edge::front_of(|mut stream| {
                read_route(&mut stream).unwrap();
            }));
            // The relay takes the handshake's start, and answers with the header of a TLS record
            // alone, as a server's answer cut short.
            let cut = once(edge::front_of(|mut stream| {
                read_route(&mut stream).unwrap();
                stream.read_exact(&mut [0; 5]).unwrap();
                stream.write_all(&[0x16, 0x03, 0x03, 0x00, 0x40]).unwrap();
            }));
            vec![
                format!("a relay that closes before the route: {unrouted}"),
                format!("one that closes at the route: {refused}"),
                format!("one that closes in the companion's handshake: {cut}"),
            ]
        },
        &expect![[r#"
            a relay that closes before the route: Err("TimedOut: the gateway didn't reach companion `scheduler` through its sandbox's relay at localhost:$PORT by the deadline: its last dial failed: the relay at localhost:$PORT closed the connection before the TLS handshake with companion `scheduler` was done: expected its sandboxd to listen there while the deadline lasts")
            one that closes at the route: Err("TimedOut: the gateway didn't reach companion `scheduler` through its sandbox's relay at localhost:$PORT by the deadline: its last dial failed: the relay at localhost:$PORT closed the connection before the TLS handshake with companion `scheduler` was done: expected its sandboxd to listen there while the deadline lasts")
            one that closes in the companion's handshake: Err("TimedOut: the gateway didn't reach companion `scheduler` through its sandbox's relay at localhost:$PORT by the deadline: its last dial failed: the relay at localhost:$PORT closed the connection before the TLS handshake with companion `scheduler` was done: expected its sandboxd to listen there while the deadline lasts")"#]],
    );
}

/// What becomes of the accept of a companion that says `says` within its TLS, on `clock`.
fn greeted<C: Clock>(dialer: &Dialer, says: Says, (clock, by): (&C, Duration)) -> String {
    let (grant, identity, unreached) = relayed_to(dialer, "scheduler");
    let server = relayed::server(&identity);
    let (port, relay) = relayed_behind("scheduler", server, says);
    let outcome = accepted(unreached, port, (clock, by), false);
    assert!(ended_unread(&relay, &grant));
    outcome
}

#[test]
fn unmet_companions() {
    check(
        |_| {
            let dialer = Dialer::new(edge::client());
            // The relay holds the connection and carries nothing, until the gateway ends it.
            let (stalled, ended) = (Arc::new(AtomicBool::new(false)), mpsc::channel());
            let (reached, (end, ends)) = (Arc::clone(&stalled), ended);
            let port = edge::front_of(move |mut stream| {
                read_route(&mut stream).unwrap();
                reached.store(true, Ordering::Relaxed);
                let _ = stream.read_to_end(&mut Vec::new());
                let _ = end.send(());
            });
            let (_, _, unreached) = relayed_to(&dialer, "scheduler");
            let clock = Still::until(stalled);
            let uncarried = accepted(unreached, port, (&clock, STALL_BOUND), false);
            ends.recv_timeout(HUNG).unwrap();
            let stalled = Arc::new(AtomicBool::new(false));
            let clock = Still::until(Arc::clone(&stalled));
            let silent = greeted(&dialer, Says::Nothing(stalled), (&clock, STALL_BOUND));
            vec![
                format!("a relay that carries nothing: {uncarried}"),
                format!("a companion that says no hello: {silent}"),
            ]
        },
        &expect![[r#"
            a relay that carries nothing: Err("TimedOut: the gateway didn't reach companion `scheduler` through its sandbox's relay at localhost:$PORT by the deadline: its last dial failed: companion `scheduler` behind the relay at localhost:$PORT: the TLS handshake wasn't done by its deadline: expected TLS 1.3 from `scheduler`, with a certificate trusted for it: expected its sandboxd to listen there while the deadline lasts")
            a companion that says no hello: Err("TimedOut: the gateway didn't reach companion `scheduler` through its sandbox's relay at localhost:$PORT by the deadline: its last dial failed: the sandbox connected but sent no hello by the deadline: expected its sandboxd to send one as it connects, as a sandboxd built from the same Pumpkin does: expected its sandboxd to listen there while the deadline lasts")"#]],
    );
}
