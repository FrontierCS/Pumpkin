//! The model upstream, against a stand-in model provider: a local HTTP server that answers each
//! request as its case says. The test is the session's owner: it makes each case's upstream,
//! serves the harness's request from the fake sandbox with it, and shows what the stand-in
//! received and what the sandbox heard.

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::{fs, iter, thread};

use expect_test::{Expect, expect};
use pumpkin_gateway::{Arrival, Listener};
use pumpkin_model_route::{Answer as Told, Key, Route, Upstream, UpstreamUrl};
use stdx::Deadline;

use super::{HUNG, fake_sandbox, models, next_model_request, post};

/// The key file's text, whose key goes upstream.
const KEY: &str = "sk-test-0123\n";

/// The stand-in's URL, which [`check`] completes with its address.
const STAND_IN: &str = "http://{stand-in}/v1/responses";

/// The head of the stand-in's stream, after its status line: headers that a harness reads, then
/// ones that stay behind.
const STREAM_HEAD: &str = "content-type: text/event-stream\r\nopenai-model: m-large\r\n\
                           x-request-id: req_1\r\nx-codex-primary-used-percent: 5\r\n\
                           openai-organization: org-1\r\nopenai-project: proj_1\r\n\
                           x-ratelimit-remaining-requests: 499\r\nset-cookie: s=1\r\n\
                           transfer-encoding: chunked\r\nconnection: close\r\n\r\n";

/// The two parts of the stand-in's streamed body.
const PARTS: [&str; 2] = [
    "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
    "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n",
];

/// A case: its name, its upstream's URL and the text of its key file (`None` for no file), how
/// the stand-in answers (`None` if it never receives the request), and whether a second request,
/// which the stand-in never receives, is served with the same upstream (`Some(allowed)`, as the
/// work phase has not ended or has).
struct Case {
    name: &'static str,
    url: &'static str,
    key: Option<&'static str>,
    answer: Option<Answer>,
    then: Option<bool>,
}

/// How the stand-in answers a case's request.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// With a stream in two parts, the second once the sandbox has heard the first.
    Streams,
    /// With an error of its own, a 429.
    Refuses,
    /// With a refusal, of the key or the request: a 401 or a 403.
    RefusesKey(u16),
    /// With a redirect to itself.
    Redirects,
    /// With a body that it codes, though the request asks for it uncoded.
    Codes,
    /// With the stream's head and first part, then it hangs up.
    BreaksOff,
    /// It hangs up without an answer.
    HangsUp,
}

/// A case whose upstream is the stand-in with [`KEY`], never given up on.
fn case(name: &'static str, answer: Answer) -> Case {
    Case {
        name,
        url: STAND_IN,
        key: Some(KEY),
        answer: Some(answer),
        then: None,
    }
}

/// A case whose request the stand-in never receives.
fn unanswered(name: &'static str) -> Case {
    Case {
        answer: None,
        ..case(name, Answer::Streams)
    }
}

/// Makes each case's upstream, and through each that it makes, forwards the harness's request,
/// sent by a sandbox of a role whose route allows `m-large`, to the stand-in, answering by a
/// deadline that only a hang reaches. Checks what came of each against `expect`.
fn check(cases: &[Case], expect: &Expect) {
    let dir = std::env::temp_dir().join(format!(
        "pumpkin-upstream-{}-{}",
        std::process::id(),
        thread::current().name().unwrap()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let stand_in = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = stand_in.local_addr().unwrap().to_string();
    let made: Vec<Result<Upstream, String>> = (0..)
        .zip(cases)
        .map(|(n, case)| upstream(case, &dir.join(format!("key-{n}")), &address))
        .collect();
    let forwarded: Vec<&Case> = cases
        .iter()
        .zip(&made)
        .filter_map(|(case, made)| made.is_ok().then_some(case))
        .collect();
    let answers = forwarded.iter().filter_map(|case| case.answer).collect();
    let (parts, heard_part) = mpsc::channel();
    let stand_in = thread::spawn(move || serve(&stand_in, answers, &heard_part));
    let socket = dir.join("gateway.sock");
    let listener = Listener::bind(socket.clone()).unwrap();
    let requests = (forwarded.iter())
        .flat_map(|case| iter::repeat_n(post(|_| {}), 1 + usize::from(case.then.is_some())))
        .collect();
    let sandbox = thread::spawn(move || fake_sandbox(&socket, requests, Some(&parts)));
    let (sender, arrivals) = mpsc::channel();
    let accepted = listener.accept(Deadline::after(HUNG), None, true, sender, |arrival| arrival);
    let session = accepted.unwrap();
    let route = Route::new(models(["m-large"]));
    let sent = post(|_| {}).3;
    let mut outcomes = Vec::new();
    for (case, upstream) in cases.iter().zip(&made) {
        let Ok(upstream) = upstream else { continue };
        let served = serve_case(case, upstream, &route, &arrivals);
        outcomes.push((served, upstream.key_was_refused()));
    }
    let mut heard = sandbox.join().unwrap().into_iter();
    let mut received = stand_in.join().unwrap().into_iter();
    let mut outcomes = outcomes.into_iter();
    let mut first = None;
    let lines: Vec<String> = cases
        .iter()
        .zip(made)
        .map(|(case, made)| {
            let Err(refused) = made else {
                let (served, key_refused) = outcomes.next().unwrap();
                let got = match case.answer {
                    Some(_) => got(received.next().unwrap(), &sent, &mut first),
                    None => "nothing".to_owned(),
                };
                let mut line = format!("{}: the stand-in got {got}", case.name);
                for told in served {
                    let heard = heard.next().unwrap().replace('\n', "\\n");
                    write!(line, "; the sandbox heard {heard}; told: {told:?}").unwrap();
                }
                write!(line, "; key refused: {key_refused}").unwrap();
                return line;
            };
            format!("{}: {refused}", case.name)
        })
        .collect();
    let shown = lines.join("\n").replace(&address, "<stand-in>");
    expect.assert_eq(&shown.replace(&dir.display().to_string(), "<dir>"));
    drop(session);
    fs::remove_dir_all(dir).unwrap();
}

/// Serves `case`'s request, and its second if it has one, with `upstream`, and returns how it
/// answered each.
fn serve_case(
    case: &Case,
    upstream: &Upstream,
    route: &Route,
    arrivals: &Receiver<Arrival>,
) -> Vec<Option<Told>> {
    let by = Deadline::after(HUNG);
    let mut served = vec![serve_one(upstream, route, arrivals, true, by)];
    if let Some(allowed) = case.then {
        served.push(serve_one(upstream, route, arrivals, allowed, by));
    }
    served
}

/// Serves the next model request of `arrivals` with `upstream`, and returns how it answered.
fn serve_one(
    upstream: &Upstream,
    route: &Route,
    arrivals: &Receiver<Arrival>,
    allowed: bool,
    by: Deadline,
) -> Option<Told> {
    let incoming = next_model_request(arrivals);
    let mut told = None;
    upstream.serve(
        route,
        incoming,
        allowed,
        by,
        || {},
        |answer| told = Some(answer),
    );
    told
}

/// The upstream that `case` names, with its key file at `key`, or why it can't be made.
fn upstream(case: &Case, key: &Path, address: &str) -> Result<Upstream, String> {
    if let Some(text) = case.key {
        fs::write(key, text).unwrap();
    }
    let url = UpstreamUrl::try_from(case.url.replace("{stand-in}", address))?;
    Ok(Upstream::new(url, Key::read(key)?))
}

/// What the stand-in received, as `(head, body)`: a head that is the `first` it received shown
/// as such, and a body that is the one `sent` shown as such.
fn got((head, body): (String, Vec<u8>), sent: &[u8], first: &mut Option<String>) -> String {
    let head = if first.is_none() {
        first.insert(head).clone()
    } else if first.as_ref() == Some(&head) {
        "the same head".to_owned()
    } else {
        head
    };
    if body == sent {
        format!("{head} with the body as sent")
    } else {
        format!("{head} with the body {}", String::from_utf8_lossy(&body))
    }
}

/// Answers a request on each connection in turn, as `answers` say, and returns what each held:
/// its line and headers, and its body. It sends a stream's second part, or breaks it off, once
/// `heard_part` says the sandbox heard of its first.
fn serve(
    listener: &TcpListener,
    answers: Vec<Answer>,
    heard_part: &Receiver<()>,
) -> Vec<(String, Vec<u8>)> {
    let mut received = Vec::new();
    for answer in answers {
        let (mut connection, _) = listener.accept().unwrap();
        let (line, headers, body) = read_request(&mut BufReader::new(&connection));
        received.push((format!("{line} {headers:?}"), body));
        // What the sandbox heard of earlier answers.
        while heard_part.try_recv().is_ok() {}
        match answer {
            Answer::Streams | Answer::BreaksOff => {
                write!(connection, "HTTP/1.1 200 OK\r\n{STREAM_HEAD}").unwrap();
                write_chunk(&mut connection, PARTS[0]);
                let heard = heard_part.recv_timeout(HUNG);
                assert!(
                    heard.is_ok(),
                    "the sandbox heard nothing of the stream's first part"
                );
                if answer == Answer::Streams {
                    write_chunk(&mut connection, PARTS[1]);
                    write_chunk(&mut connection, "");
                }
            }
            Answer::Refuses => {
                let body = r#"{"error":{"message":"slow down","type":"requests"}}"#;
                let head = format!(
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\n\
                     retry-after: 2\r\nopenai-organization: org-1\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                connection.write_all((head + body).as_bytes()).unwrap();
            }
            Answer::RefusesKey(status) => {
                let body = "Authorization failed: expired token";
                let head = format!(
                    "HTTP/1.1 {status} Refused\r\ncontent-type: text/plain\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                connection.write_all((head + body).as_bytes()).unwrap();
            }
            Answer::Codes => {
                let body = "a body coded as gzip";
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                     content-encoding: gzip\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                connection.write_all((head + body).as_bytes()).unwrap();
            }
            Answer::Redirects => {
                let address = listener.local_addr().unwrap();
                let head = format!(
                    "HTTP/1.1 307 Temporary Redirect\r\nlocation: http://{address}/v2/responses\r\n\
                     content-length: 0\r\nconnection: close\r\n\r\n"
                );
                connection.write_all(head.as_bytes()).unwrap();
            }
            Answer::HangsUp => {}
        }
        let _ = connection.shutdown(Shutdown::Both);
    }
    received
}

/// Writes `text` as one chunk of a chunked body: the last, if it is empty.
pub(crate) fn write_chunk(connection: &mut TcpStream, text: &str) {
    write!(connection, "{:x}\r\n{text}\r\n", text.len()).unwrap();
}

/// A request's line, its headers and its body.
pub(crate) fn read_request(reader: &mut impl BufRead) -> (String, Vec<(String, String)>, Vec<u8>) {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut headers = Vec::new();
    let mut length = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        let Some((name, value)) = header.trim_end().split_once(": ") else {
            break;
        };
        if name == "content-length" {
            length = value.parse().unwrap();
        }
        headers.push((name.to_owned(), value.to_owned()));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    (line.trim_end().to_owned(), headers, body)
}

/// The harness's request reaches the stand-in with only the headers that pass, the key and the
/// closed body; its response streams back as it arrives, with only the headers that a harness
/// reads, and is cut short if the stand-in breaks it off or stalls until it is given up on; and
/// the sandbox hears a 502 or a 504 if the stand-in gives no answer, a 502 if it codes the body,
/// and a 400 if it refuses the key or the request. The forward says how it answered each. An
/// upstream whose URL or key file is refused can't be made.
#[test]
fn forward() {
    check(
        &[
            case("a stream", Answer::Streams),
            case("the provider's refusal", Answer::Refuses),
            Case {
                then: Some(true),
                ..case("the key refused", Answer::RefusesKey(401))
            },
            Case {
                then: Some(false),
                ..case("the work phase ended", Answer::Refuses)
            },
            case("a request forbidden", Answer::RefusesKey(403)),
            case("a redirect", Answer::Redirects),
            case("a coded body", Answer::Codes),
            case("a stream broken off", Answer::BreaksOff),
            case("a hang-up", Answer::HangsUp),
            // Each of these is on this host, though `0.0.0.0` is not a loopback address, so that
            // a rule broken would show without reaching the network.
            Case {
                url: "http://0.0.0.0:9/v1/responses",
                ..unanswered("plain http beyond the loopback")
            },
            Case {
                url: "https://user:secret@127.0.0.1:9/v1/responses",
                ..unanswered("a user")
            },
            Case {
                url: "http://127.0.0.1:99999/v1/responses",
                ..unanswered("a port beyond 65535")
            },
            Case {
                url: "ftp://127.0.0.1:9/v1/responses",
                ..unanswered("another scheme")
            },
            Case {
                url: "http://127.0.0.1:9/v1/re sponses",
                ..unanswered("not a URL")
            },
            Case {
                key: None,
                ..unanswered("no key file")
            },
            Case {
                key: Some(" \n"),
                ..unanswered("an empty key file")
            },
            Case {
                key: Some("sk-test 0123\n"),
                ..unanswered("a key with a space")
            },
        ],
        &expect![[r#"
            a stream: the stand-in got POST /v1/responses HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "552"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "text/event-stream"), ("originator", "codex_exec"), ("user-agent", "codex_exec/0.167.0"), ("session-id", "s-1"), ("thread-id", "t-1"), ("x-codex-beta-features", "remote_compaction_v2"), ("x-codex-window-id", "w-1"), ("x-codex-turn-metadata", "{\"turn_id\":\"u-1\"}"), ("x-openai-internal-codex-responses-lite", "true"), ("authorization", "Bearer sk-test-0123")] with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\nevent: response.completed\ndata: {"type":"response.completed"}\n\n; told: Some(Forwarded { status: 200, request_id: Some("req_1") }); key refused: false
            the provider's refusal: the stand-in got the same head with the body as sent; the sandbox heard 429 [("content-type", "application/json"), ("retry-after", "2")] {"error":{"message":"slow down","type":"requests"}}; told: Some(Forwarded { status: 429, request_id: None }); key refused: false
            the key refused: the stand-in got the same head with the body as sent; the sandbox heard 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider refused the route's key, with a 401, which ends the model route: make no more model requests","param":null,"type":"invalid_request_error"}}; told: Some(Refused { upstream_status: Some(401), request_id: None, status: 400, message: "the model provider refused the route's key, with a 401, which ends the model route: make no more model requests" }); the sandbox heard 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider refused the route's key, with a 401, which ends the model route: make no more model requests","param":null,"type":"invalid_request_error"}}; told: Some(Barred { status: 400, message: "the model provider refused the route's key, with a 401, which ends the model route: make no more model requests" }); key refused: true
            the work phase ended: the stand-in got the same head with the body as sent; the sandbox heard 429 [("content-type", "application/json"), ("retry-after", "2")] {"error":{"message":"slow down","type":"requests"}}; told: Some(Forwarded { status: 429, request_id: None }); the sandbox heard 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the work phase has ended, and with it the model route: make no more model requests","param":null,"type":"invalid_request_error"}}; told: Some(Barred { status: 400, message: "the work phase has ended, and with it the model route: make no more model requests" }); key refused: false
            a request forbidden: the stand-in got the same head with the body as sent; the sandbox heard 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider refused the request, with a 403: send only requests that it allows","param":null,"type":"invalid_request_error"}}; told: Some(Refused { upstream_status: Some(403), request_id: None, status: 400, message: "the model provider refused the request, with a 403: send only requests that it allows" }); key refused: false
            a redirect: the stand-in got the same head with the body as sent; the sandbox heard 307 [] ; told: Some(Forwarded { status: 307, request_id: None }); key refused: false
            a coded body: the stand-in got the same head with the body as sent; the sandbox heard 502 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider coded its answer, though the request asked for it uncoded: try again","param":null,"type":"server_error"}}; told: Some(Refused { upstream_status: Some(200), request_id: None, status: 502, message: "the model provider coded its answer, though the request asked for it uncoded: try again" }); key refused: false
            a stream broken off: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\n, cut short; told: Some(Cut { status: 200, request_id: Some("req_1") }); key refused: false
            a hang-up: the stand-in got the same head with the body as sent; the sandbox heard 502 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider gave no answer: try again","param":null,"type":"server_error"}}; told: Some(Refused { upstream_status: None, request_id: None, status: 502, message: "the model provider gave no answer: try again" }); key refused: false
            plain http beyond the loopback: the upstream's URL is plain http to a host beyond the loopback: name an https URL without a user, or an http URL to a loopback IP address such as 127.0.0.1
            a user: the upstream's URL names a user: name an https URL without a user, or an http URL to a loopback IP address such as 127.0.0.1
            a port beyond 65535: the upstream's URL names a port that is not a number up to 65535: name one, or none for the scheme's default
            another scheme: the upstream's URL is not an http URL: name an https URL without a user, or an http URL to a loopback IP address such as 127.0.0.1
            not a URL: the upstream's URL is not a URL (invalid uri character): name an https URL without a user, or an http URL to a loopback IP address such as 127.0.0.1
            no key file: the key file <dir>/key-14 can't be read (No such file or directory (os error 2)): name a file that holds the model provider's key
            an empty key file: the key file <dir>/key-15 holds no API key: write the key alone, on one line
            a key with a space: the key file <dir>/key-16 holds no API key: write the key alone, on one line"#]],
    );
}
