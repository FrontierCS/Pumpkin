//! The model upstream, against a stand-in model provider: a local HTTP server that answers each
//! request as its case says. The test is the session's owner: it makes each case's upstream,
//! closes the harness's request from the fake sandbox and forwards it, and shows what the
//! stand-in received and what the sandbox heard.

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use expect_test::{Expect, expect};
use pumpkin::model_route::{Key, Route, Upstream, UpstreamUrl};
use pumpkin_gateway::Listener;
use pumpkin_schema::ir::Models;
use stdx::Deadline;

use super::{HUNG, fake_sandbox, next_model_request, post};

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
/// the stand-in answers (`None` if it never receives the request), and how long the upstream may
/// take before it is given up on.
struct Case {
    name: &'static str,
    url: &'static str,
    key: Option<&'static str>,
    answer: Option<Answer>,
    until: Duration,
}

/// How the stand-in answers a case's request.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// With a stream in two parts, the second once the sandbox has heard the first.
    Streams,
    /// With an error of its own, a 429.
    Refuses,
    /// With a redirect to itself.
    Redirects,
    /// With a body that it codes, though the request asks for it uncoded.
    Codes,
    /// With the stream's head and first part, then it hangs up.
    BreaksOff,
    /// With the stream's head and first part, then nothing more, until its client hangs up.
    StallsAfterPart,
    /// It hangs up without an answer.
    HangsUp,
    /// It answers nothing, until its client hangs up.
    StaysSilent,
}

/// A case whose upstream is the stand-in with [`KEY`], never given up on.
fn case(name: &'static str, answer: Answer) -> Case {
    Case {
        name,
        url: STAND_IN,
        key: Some(KEY),
        answer: Some(answer),
        until: Duration::MAX,
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
    let dir = std::env::temp_dir().join(format!("pumpkin-upstream-{}", std::process::id()));
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
    let requests = forwarded.iter().map(|_| post(|_| {})).collect();
    let sandbox = thread::spawn(move || fake_sandbox(&socket, requests, Some(&parts)));
    let (sender, arrivals) = mpsc::channel();
    let accepted = listener.accept(Deadline::after(HUNG), None, true, sender, |arrival| arrival);
    let session = accepted.unwrap();
    let models: Models = serde_json::from_value(serde_json::json!(["m-large"])).unwrap();
    let route = Route::new(models);
    let mut outcomes = Vec::new();
    for (case, upstream) in cases.iter().zip(&made) {
        let Ok(upstream) = upstream else { continue };
        let incoming = next_model_request(&arrivals);
        let closed = route.close(&incoming.request).unwrap();
        let until = Deadline::after(case.until);
        let forwarded = upstream.forward(&closed, incoming, until, Deadline::after(HUNG));
        outcomes.push((
            closed.body().to_vec(),
            forwarded.map_err(|error| error.kind()),
        ));
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
                let (closed, forwarded) = outcomes.next().unwrap();
                let got = match case.answer {
                    Some(_) => got(received.next().unwrap(), &closed, &mut first),
                    None => "nothing".to_owned(),
                };
                let heard = heard.next().unwrap().replace('\n', "\\n");
                return format!(
                    "{}: the stand-in got {got}; the sandbox heard {heard}; forwarding: \
                     {forwarded:?}",
                    case.name
                );
            };
            format!("{}: {refused}", case.name)
        })
        .collect();
    let shown = lines.join("\n").replace(&address, "<stand-in>");
    expect.assert_eq(&shown.replace(&dir.display().to_string(), "<dir>"));
    drop(session);
    fs::remove_dir_all(dir).unwrap();
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
/// as such, and a body that is `closed` shown as such.
fn got((head, body): (String, Vec<u8>), closed: &[u8], first: &mut Option<String>) -> String {
    let head = if first.is_none() {
        first.insert(head).clone()
    } else if first.as_ref() == Some(&head) {
        "the same head".to_owned()
    } else {
        head
    };
    if body == closed {
        format!("{head} with the closed body")
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
        received.push(read_request(&mut BufReader::new(&connection)));
        // What the sandbox heard of earlier answers.
        while heard_part.try_recv().is_ok() {}
        match answer {
            Answer::StaysSilent => {
                io::copy(&mut connection, &mut io::sink()).unwrap();
            }
            Answer::StallsAfterPart => {
                write!(connection, "HTTP/1.1 200 OK\r\n{STREAM_HEAD}").unwrap();
                write_chunk(&mut connection, PARTS[0]);
                io::copy(&mut connection, &mut io::sink()).unwrap();
            }
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
fn write_chunk(connection: &mut TcpStream, text: &str) {
    write!(connection, "{:x}\r\n{text}\r\n", text.len()).unwrap();
}

/// A request's line and headers, and its body.
fn read_request(reader: &mut impl BufRead) -> (String, Vec<u8>) {
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
    (format!("{} {headers:?}", line.trim_end()), body)
}

/// The harness's request reaches the stand-in with only the headers that pass, the key and the
/// closed body; its response streams back as it arrives, with only the headers that a harness
/// reads, and is cut short if the stand-in breaks it off or stalls until it is given up on; and
/// the sandbox hears a 502 or a 504 if the stand-in gives no answer, and a 502 if it codes the
/// body. An upstream whose URL or key file is refused can't be made.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn forward() {
    check(
        &[
            case("a stream", Answer::Streams),
            case("the provider's refusal", Answer::Refuses),
            case("a redirect", Answer::Redirects),
            case("a coded body", Answer::Codes),
            case("a stream broken off", Answer::BreaksOff),
            Case {
                until: Duration::from_millis(100),
                ..case(
                    "a stream stalled until it is given up on",
                    Answer::StallsAfterPart,
                )
            },
            case("a hang-up", Answer::HangsUp),
            Case {
                until: Duration::from_millis(100),
                ..case("silence until it is given up on", Answer::StaysSilent)
            },
            Case {
                until: Duration::ZERO,
                ..unanswered("given up on before it is sent")
            },
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
            a stream: the stand-in got POST /v1/responses HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "552"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "text/event-stream"), ("originator", "codex_exec"), ("user-agent", "codex_exec/0.167.0"), ("session-id", "s-1"), ("thread-id", "t-1"), ("x-codex-beta-features", "remote_compaction_v2"), ("x-codex-window-id", "w-1"), ("x-codex-turn-metadata", "{\"turn_id\":\"u-1\"}"), ("x-openai-internal-codex-responses-lite", "true"), ("authorization", "Bearer sk-test-0123")] with the closed body; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\nevent: response.completed\ndata: {"type":"response.completed"}\n\n; forwarding: Ok(())
            the provider's refusal: the stand-in got the same head with the closed body; the sandbox heard 429 [("content-type", "application/json"), ("retry-after", "2")] {"error":{"message":"slow down","type":"requests"}}; forwarding: Ok(())
            a redirect: the stand-in got the same head with the closed body; the sandbox heard 307 [] ; forwarding: Ok(())
            a coded body: the stand-in got the same head with the closed body; the sandbox heard 502 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider coded its answer, though the request asked for it uncoded: try again","param":null,"type":"server_error"}}; forwarding: Ok(())
            a stream broken off: the stand-in got the same head with the closed body; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\n, cut short; forwarding: Ok(())
            a stream stalled until it is given up on: the stand-in got the same head with the closed body; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\n, cut short; forwarding: Ok(())
            a hang-up: the stand-in got the same head with the closed body; the sandbox heard 502 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider gave no answer: try again","param":null,"type":"server_error"}}; forwarding: Ok(())
            silence until it is given up on: the stand-in got the same head with the closed body; the sandbox heard 504 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider gave no answer in time: try again","param":null,"type":"server_error"}}; forwarding: Ok(())
            given up on before it is sent: the stand-in got nothing; the sandbox heard 504 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider gave no answer in time: try again","param":null,"type":"server_error"}}; forwarding: Ok(())
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
