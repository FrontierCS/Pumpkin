//! The model upstream, against a stand-in model provider: a local HTTP server that answers each
//! request as its case says. The test is the session's owner: it makes each case's upstream,
//! serves the harness's request from the fake sandbox with it, and shows what the stand-in
//! received and what the sandbox heard.

use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::io::{self, BufReader, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::{fs, iter, slice, thread};

use expect_test::{Expect, expect};
use gateway::{Arrival, IncomingModelRequest};
use route::{Answer as Told, Apart, Keep, Key, Route, Upstream, UpstreamUrl};
use schema::ir::Api;
use stdx::Deadline;
use test_support::HUNG;
use test_support::http::{read_request, write_chunk};

use super::kept::{CHAT_STREAM, RESPONSES_CUT, RESPONSES_STREAM};
use super::read::CHAT_CUSTOM;
use super::{Request, chat, fake_sandbox, meeting, models, next_model_request, post};

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

type Parts = [&'static str; 2];

const PARTS: Parts = [
    "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
    "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":12,\"input_tokens_details\":{\"cached_tokens\":8},\"output_tokens\":5,\"output_tokens_details\":{\"reasoning_tokens\":3}}}}\n\n",
];

/// The second part holds the usage that the request asked for.
const CHAT_PARTS: Parts = [
    "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"7\"}}],\"usage\":null}\n\n",
    "data: {\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":20,\"prompt_tokens_details\":{\"cached_tokens\":16},\"completion_tokens\":1,\"completion_tokens_details\":{\"reasoning_tokens\":0}}}\n\ndata: [DONE]\n\n",
];

/// A stream that fails with a rate limit as a provider sent it with a 200, its message cut short.
const LIMITED_PARTS: Parts = [
    "event: response.failed\ndata: {\"type\":\"response.failed\",\"sequence_number\":0,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"failed\",\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"Rate limit reached\"},\"output\":[],\"usage\":null}}\n\n",
    "event: error\ndata: {\"type\":\"error\",\"code\":\"rate_limit_exceeded\",\"message\":\"Rate limit reached\",\"param\":null,\"sequence_number\":1}\n\n",
];

/// A stream that fails after some of the model's output, reporting its usage.
const FAILED_LATE_PARTS: Parts = [
    "event: response.created\ndata: {\"type\":\"response.created\"}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n",
    "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"server_error\",\"message\":\"The model failed\"},\"usage\":{\"input_tokens\":12,\"output_tokens\":1}}}\n\n",
];

/// A stream whose failure gives no code.
const UNCODED_PARTS: Parts = [
    PARTS[0],
    "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":null}}\n\n",
];

/// A stream whose failure gives a code that is no identifier.
const MISCODED_PARTS: Parts = [
    PARTS[0],
    "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"Slow down: wait 3s\",\"message\":\"\"}}}\n\n",
];

/// A stream that ends with an `error` event alone.
const ERROR_PARTS: Parts = [
    PARTS[0],
    "event: error\ndata: {\"type\":\"error\",\"code\":\"server_error\",\"message\":\"An error occurred\",\"param\":null}\n\n",
];

/// A stream whose first item added is reasoning, and whose second, a function call, begins its
/// output.
const CALL_PARTS: Parts = [
    "event: response.created\ndata: {\"type\":\"response.created\"}\n\nevent: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"reasoning\"}}\n\n",
    "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"name\":\"exec\"}}\n\nevent: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{}\"}\n\n",
];

/// A stream of Chat Completions whose first chunk names its role with empty content, and whose
/// second begins its output with a tool call.
const CHAT_CALL_PARTS: Parts = [
    "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
    "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"exec\",\"arguments\":\"\"}}]}}]}\n\ndata: [DONE]\n\n",
];

/// A stream whose first item added is reasoning, as real streams' are, whose first delta is empty,
/// and whose second begins its output.
const REASONING_PARTS: Parts = [
    "event: response.created\ndata: {\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"output\":[],\"status\":\"in_progress\"},\"sequence_number\":0,\"type\":\"response.created\"}\n\nevent: response.output_item.added\ndata: {\"item\":{\"content\":[],\"id\":\"rs_1\",\"status\":\"in_progress\",\"summary\":[],\"type\":\"reasoning\"},\"output_index\":0,\"sequence_number\":1,\"type\":\"response.output_item.added\"}\n\nevent: response.reasoning_text.delta\ndata: {\"content_index\":0,\"delta\":\"\",\"item_id\":\"rs_1\",\"output_index\":0,\"sequence_number\":2,\"type\":\"response.reasoning_text.delta\"}\n\n",
    "event: response.reasoning_text.delta\ndata: {\"content_index\":0,\"delta\":\"Two\",\"item_id\":\"rs_1\",\"output_index\":0,\"sequence_number\":3,\"type\":\"response.reasoning_text.delta\"}\n\n",
];

/// A stream whose output begins with a delta of a call's arguments.
const ARGUMENTS_PARTS: Parts = [
    PARTS[0],
    "event: response.function_call_arguments.delta\ndata: {\"delta\":\"{\\\"\",\"item_id\":\"fc_1\",\"output_index\":0,\"sequence_number\":1,\"type\":\"response.function_call_arguments.delta\"}\n\n",
];

/// A stream whose output begins with a custom tool call's item added, as Codex's `apply_patch`
/// streams.
const CUSTOM_CALL_PARTS: Parts = [
    "event: response.created\ndata: {\"type\":\"response.created\"}\n\nevent: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"custom_tool_call\",\"id\":\"ctc_1\",\"call_id\":\"call_1\",\"name\":\"apply_patch\",\"input\":\"\",\"status\":\"in_progress\"}}\n\n",
    "event: response.custom_tool_call_input.delta\ndata: {\"type\":\"response.custom_tool_call_input.delta\",\"item_id\":\"ctc_1\",\"call_id\":\"call_1\",\"delta\":\"*** Begin Patch\"}\n\n",
];

/// A stream whose output begins with a delta of a custom tool call's input.
const CUSTOM_INPUT_PARTS: Parts = [PARTS[0], CUSTOM_CALL_PARTS[1]];

/// A stream whose output begins with a delta of a refusal.
const REFUSAL_PARTS: Parts = [
    PARTS[0],
    "event: response.refusal.delta\ndata: {\"type\":\"response.refusal.delta\",\"delta\":\"No\"}\n\n",
];

/// A stream of Chat Completions whose first chunk begins its output with reasoning, as real
/// streams' do.
const CHAT_REASONING_PARTS: Parts = [
    "data: {\"id\":\"chatcmpl_1\",\"created\":1,\"object\":\"chat.completion.chunk\",\"model\":\"m-large\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"Two\"}}]}\n\n",
    "data: [DONE]\n\n",
];

/// A stream of Chat Completions whose reasoning is named `reasoning`, empty in its first chunk.
const CHAT_REASONED_PARTS: Parts = [
    "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\",\"reasoning\":\"\"}}]}\n\n",
    "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"Two\"}}]}\n\ndata: [DONE]\n\n",
];

/// A stream of Chat Completions that fails after its first chunk.
const CHAT_FAILED_PARTS: Parts = [
    CHAT_PARTS[0],
    "data: {\"error\":{\"message\":\"Rate limit reached\",\"type\":\"rate_limit_error\",\"param\":null,\"code\":\"rate_limit_exceeded\"}}\n\n",
];

struct Case {
    name: &'static str,
    /// The API of the route's entry, whose client's request the sandbox sends.
    api: Api,
    url: &'static str,
    /// The text of its key file; `None` for no file.
    key: Option<&'static str>,
    /// How the stand-in answers; `None` if it never receives the request.
    answer: Option<Answer>,
    /// Whether a second request, which the stand-in never receives, is served with the same
    /// upstream: `Some(allowed)`.
    then: Option<bool>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// With a stream in two parts, the second once the sandbox has heard the first.
    Streams(Parts),
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
    /// With a stream in two parts, whose sandbox hangs up as it hears the first: the second, and
    /// the stream's end, come then.
    Unheard,
    /// It hangs up without an answer.
    HangsUp,
    /// It tells the test that it holds the request, then holds it without an answer until the
    /// route's end hangs up on it.
    Holds,
}

/// A case whose upstream is the stand-in with [`KEY`], never given up on.
fn case(name: &'static str, answer: Answer) -> Case {
    Case {
        name,
        api: Api::Responses,
        url: STAND_IN,
        key: Some(KEY),
        answer: Some(answer),
        then: None,
    }
}

/// A case of Chat Completions, whose upstream is the stand-in with [`KEY`], never given up on.
fn chat_case(name: &'static str, answer: Answer) -> Case {
    Case {
        api: Api::Chat,
        url: "http://{stand-in}/v1/chat/completions",
        ..case(name, answer)
    }
}

/// A case whose request the stand-in never receives.
fn unanswered(name: &'static str) -> Case {
    Case {
        answer: None,
        ..case(name, Answer::Streams(PARTS))
    }
}

/// Checks what came of each case against `expect`.
fn check(cases: &[Case], expect: &Expect) {
    expect.assert_eq(&exchanged(cases));
}

/// Makes each case's upstream, and through each that it makes, forwards the request of a client
/// of the case's API, sent by a sandbox whose route allows `m-large` and `m-small`, to the
/// stand-in, answering by a deadline that only a hang reaches, and shows what came of each, a
/// line each.
#[expect(
    clippy::too_many_lines,
    reason = "it runs a session, a stand-in and each case's exchange in one place"
)]
fn exchanged(cases: &[Case]) -> String {
    // Kept if the test fails.
    let dir = tempfile::Builder::new()
        .prefix(&format!("route-{}-", thread::current().name().unwrap()))
        .tempdir()
        .unwrap()
        .keep();
    let stand_in = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = stand_in.local_addr().unwrap().to_string();
    let made: Vec<Result<(Route, Upstream), String>> = (0..)
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
    let (holds, held) = mpsc::channel();
    let stand_in = thread::spawn(move || serve(&stand_in, answers, &heard_part, &holds));
    // Its sockets fit in a socket's address by this path to their directory.
    let (_sockets, registration, socket) = meeting(&dir);
    let mut requests = Vec::new();
    let mut hangs_up = None;
    for case in &forwarded {
        if case.answer == Some(Answer::Unheard) {
            hangs_up = Some(u32::try_from(requests.len()).unwrap());
        }
        let count = 1 + usize::from(case.then.is_some());
        requests.extend(iter::repeat_n(request(case.api), count));
    }
    let sandbox = thread::spawn(move || fake_sandbox(&socket, requests, Some(&parts), hangs_up));
    let (sender, arrivals) = mpsc::channel();
    let by = Deadline::after(HUNG);
    let accepted = registration.accept(by, None, true, sender, |arrival| arrival, || false);
    let session = accepted.unwrap();
    let mut outcomes = Vec::new();
    for (case, entry) in cases.iter().zip(&made) {
        let Ok(entry) = entry else { continue };
        let served = serve_case(case, entry, &arrivals, &held);
        outcomes.push((served, entry.1.key_was_refused()));
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
                    Some(_) => {
                        let sent = request(case.api).3;
                        got(received.next().unwrap(), &sent, &mut first)
                    }
                    None => "nothing".to_owned(),
                };
                let mut line = format!("{}: the stand-in got {got}", case.name);
                for (told, kept) in served {
                    let heard = heard.next().unwrap().replace('\n', "\\n");
                    write!(
                        line,
                        "; the sandbox heard {heard}; told: {told:?}; kept: {kept}"
                    )
                    .unwrap();
                }
                write!(line, "; key refused: {key_refused}").unwrap();
                return line;
            };
            format!("{}: {refused}", case.name)
        })
        .collect();
    let shown = lines.join("\n").replace(&address, "<stand-in>");
    drop(session);
    fs::remove_dir_all(&dir).unwrap();
    shown.replace(&dir.display().to_string(), "<dir>")
}

/// Serves `case`'s request, and its second if it has one, with `entry`, and returns how it
/// answered each. For a case whose stand-in holds the request, the route ends once `held` says
/// that it does.
fn serve_case(
    case: &Case,
    entry: &(Route, Upstream),
    arrivals: &Receiver<Arrival>,
    held: &Receiver<()>,
) -> Vec<(Option<Told>, String)> {
    let by = Deadline::after(HUNG);
    let incoming = next_model_request(arrivals);
    let first = if case.answer == Some(Answer::Holds) {
        thread::scope(|scope| {
            let served = scope.spawn(move || serve_one(entry, incoming, true, by));
            held.recv_timeout(HUNG).unwrap();
            entry.1.shutdown();
            served.join().unwrap()
        })
    } else {
        serve_one(entry, incoming, true, by)
    };
    let mut served = vec![first];
    if let Some(allowed) = case.then {
        let incoming = next_model_request(arrivals);
        served.push(serve_one(entry, incoming, allowed, by));
    }
    served
}

/// Serves `incoming` with `entry`, a route's only one, and returns how it answered, and the
/// sizes of what was kept of the exchange by then.
fn serve_one(
    entry: &(Route, Upstream),
    incoming: IncomingModelRequest,
    allowed: bool,
    by: Deadline,
) -> (Option<Told>, String) {
    let kept = Kept::default();
    let mut told = None;
    let answered = |_, answer| told = Some((answer, kept.sizes()));
    route::serve(
        slice::from_ref(entry),
        incoming,
        allowed,
        by,
        &kept,
        || {},
        answered,
    );
    let (told, sizes) = told.unzip();
    (told, sizes.unwrap_or_default())
}

#[derive(Default)]
struct Kept {
    request: RefCell<Vec<u8>>,
    response: RefCell<Vec<u8>>,
    /// How many of the response's bytes were kept as its output began, once it has.
    first_output: Cell<Option<usize>>,
}

impl Kept {
    fn sizes(&self) -> String {
        let (request, response) = (self.request.borrow().len(), self.response.borrow().len());
        let mut sizes = format!("request {request} bytes, response {response} bytes");
        if let Some(bytes) = self.first_output.get() {
            write!(sizes, ", its output begun by byte {bytes}").unwrap();
        }
        sizes
    }
}

impl Keep for Kept {
    fn request(&self, body: &[u8], _: Option<Apart>) {
        self.request.borrow_mut().extend_from_slice(body);
    }

    fn response(&self, bytes: &[u8]) {
        self.response.borrow_mut().extend_from_slice(bytes);
    }

    fn first_output(&self) {
        let kept = self.response.borrow().len();
        assert!(
            self.first_output.replace(Some(kept)).is_none(),
            "told twice"
        );
    }
}

fn request(api: Api) -> Request {
    match api {
        Api::Responses => post(|_| {}),
        Api::Chat => chat(|_| {}),
    }
}

/// The route's entry for `case`'s API that allows `m-large` and `m-small`, with the upstream that
/// `case` names, its key file at `key`, or why the upstream can't be made.
fn upstream(case: &Case, key: &Path, address: &str) -> Result<(Route, Upstream), String> {
    if let Some(text) = case.key {
        fs::write(key, text).unwrap();
    }
    let url = UpstreamUrl::try_from(case.url.replace("{stand-in}", address))?;
    let route = Route::new(case.api, models(["m-large", "m-small"]));
    Ok((route, Upstream::new(&url, &Key::read(key)?)))
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
/// `heard_part` says the sandbox heard of its first, and tells `holds` of each request it holds.
#[expect(
    clippy::too_many_lines,
    reason = "it answers in each way that a case may ask for, in one match"
)]
fn serve(
    listener: &TcpListener,
    answers: Vec<Answer>,
    heard_part: &Receiver<()>,
    holds: &Sender<()>,
) -> Vec<(String, Vec<u8>)> {
    let mut received = Vec::new();
    for answer in answers {
        let (mut connection, _) = listener.accept().unwrap();
        let (line, headers, body) = read_request(&mut BufReader::new(&connection));
        received.push((format!("{line} {headers:?}"), body));
        // What the sandbox heard of earlier answers.
        while heard_part.try_recv().is_ok() {}
        match answer {
            Answer::Streams(_) | Answer::BreaksOff => {
                let parts = match answer {
                    Answer::Streams(parts) => parts,
                    _ => PARTS,
                };
                write!(connection, "HTTP/1.1 200 OK\r\n{STREAM_HEAD}").unwrap();
                write_chunk(&mut connection, parts[0]);
                let heard = heard_part.recv_timeout(HUNG);
                assert!(
                    heard.is_ok(),
                    "the sandbox heard nothing of the stream's first part"
                );
                if answer != Answer::BreaksOff {
                    write_chunk(&mut connection, parts[1]);
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
            Answer::Unheard => {
                write!(connection, "HTTP/1.1 200 OK\r\n{STREAM_HEAD}").unwrap();
                write_chunk(&mut connection, PARTS[0]);
                let heard = heard_part.recv_timeout(HUNG);
                assert!(heard.is_ok(), "the sandbox heard nothing of the first part");
                // In one write, which the route takes before it can hang up on the stand-in.
                let rest = format!("{:x}\r\n{}\r\n0\r\n\r\n", PARTS[1].len(), PARTS[1]);
                connection.write_all(rest.as_bytes()).unwrap();
            }
            Answer::HangsUp => {}
            Answer::Holds => {
                holds.send(()).unwrap();
                // Until the client hangs up.
                let _ = io::copy(&mut connection, &mut io::sink());
            }
        }
        let _ = connection.shutdown(Shutdown::Both);
    }
    received
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn forward() {
    check(
        &[
            case("a stream", Answer::Streams(PARTS)),
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
            chat_case("a stream of Chat Completions", Answer::Streams(CHAT_PARTS)),
            case("a rate limit with a 200", Answer::Streams(LIMITED_PARTS)),
            case("a failure after output", Answer::Streams(FAILED_LATE_PARTS)),
            case("a failure without a code", Answer::Streams(UNCODED_PARTS)),
            case(
                "a code that is no identifier",
                Answer::Streams(MISCODED_PARTS),
            ),
            case("an error event", Answer::Streams(ERROR_PARTS)),
            case("a function call", Answer::Streams(CALL_PARTS)),
            case("reasoning", Answer::Streams(REASONING_PARTS)),
            case("a call's arguments", Answer::Streams(ARGUMENTS_PARTS)),
            case("a custom tool call", Answer::Streams(CUSTOM_CALL_PARTS)),
            case(
                "a custom tool call's input",
                Answer::Streams(CUSTOM_INPUT_PARTS),
            ),
            case("a refusal", Answer::Streams(REFUSAL_PARTS)),
            chat_case(
                "a tool call of Chat Completions",
                Answer::Streams(CHAT_CALL_PARTS),
            ),
            chat_case(
                "reasoning content of Chat Completions",
                Answer::Streams(CHAT_REASONING_PARTS),
            ),
            chat_case(
                "reasoning of Chat Completions",
                Answer::Streams(CHAT_REASONED_PARTS),
            ),
            chat_case(
                "a failure of Chat Completions",
                Answer::Streams(CHAT_FAILED_PARTS),
            ),
        ],
        &expect![[r#"
            a stream: the stand-in got POST /v1/responses HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "552"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "text/event-stream"), ("originator", "codex_exec"), ("user-agent", "codex_exec/0.167.0"), ("session-id", "s-1"), ("thread-id", "t-1"), ("x-codex-beta-features", "remote_compaction_v2"), ("x-codex-window-id", "w-1"), ("x-codex-turn-metadata", "{\"turn_id\":\"u-1\"}"), ("x-openai-internal-codex-responses-lite", "true"), ("authorization", "Bearer sk-test-0123")] with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\nevent: response.completed\ndata: {"type":"response.completed","response":{"usage":{"input_tokens":12,"input_tokens_details":{"cached_tokens":8},"output_tokens":5,"output_tokens_details":{"reasoning_tokens":3}}}}\n\n; told: Some(Forwarded { status: 200, request_id: Some("req_1"), usage: Some(Usage { input_tokens: 12, output_tokens: 5, cached_tokens: Some(8), reasoning_tokens: Some(3) }), failed: None }); kept: request 552 bytes, response 271 bytes; key refused: false
            the provider's refusal: the stand-in got the same head with the body as sent; the sandbox heard 429 [("content-type", "application/json"), ("retry-after", "2")] {"error":{"message":"slow down","type":"requests"}}; told: Some(Forwarded { status: 429, request_id: None, usage: None, failed: None }); kept: request 552 bytes, response 51 bytes; key refused: false
            the key refused: the stand-in got the same head with the body as sent; the sandbox heard 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider refused the route's key, with a 401, which ends the model route: make no more model requests","param":null,"type":"invalid_request_error"}}; told: Some(Refused { upstream_status: Some(401), request_id: None, status: 400, message: "the model provider refused the route's key, with a 401, which ends the model route: make no more model requests" }); kept: request 552 bytes, response 35 bytes; the sandbox heard 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider refused the route's key, with a 401, which ends the model route: make no more model requests","param":null,"type":"invalid_request_error"}}; told: Some(Barred { status: 400, message: "the model provider refused the route's key, with a 401, which ends the model route: make no more model requests" }); kept: request 552 bytes, response 191 bytes; key refused: true
            the work phase ended: the stand-in got the same head with the body as sent; the sandbox heard 429 [("content-type", "application/json"), ("retry-after", "2")] {"error":{"message":"slow down","type":"requests"}}; told: Some(Forwarded { status: 429, request_id: None, usage: None, failed: None }); kept: request 552 bytes, response 51 bytes; the sandbox heard 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model route has ended: make no more model requests","param":null,"type":"invalid_request_error"}}; told: Some(Barred { status: 400, message: "the model route has ended: make no more model requests" }); kept: request 552 bytes, response 134 bytes; key refused: false
            a request forbidden: the stand-in got the same head with the body as sent; the sandbox heard 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider refused the request, with a 403: send only requests that it allows","param":null,"type":"invalid_request_error"}}; told: Some(Refused { upstream_status: Some(403), request_id: None, status: 400, message: "the model provider refused the request, with a 403: send only requests that it allows" }); kept: request 552 bytes, response 35 bytes; key refused: false
            a redirect: the stand-in got the same head with the body as sent; the sandbox heard 307 [] ; told: Some(Forwarded { status: 307, request_id: None, usage: None, failed: None }); kept: request 552 bytes, response 0 bytes; key refused: false
            a coded body: the stand-in got the same head with the body as sent; the sandbox heard 502 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider coded its answer, though the request asked for it uncoded: try again","param":null,"type":"server_error"}}; told: Some(Refused { upstream_status: Some(200), request_id: None, status: 502, message: "the model provider coded its answer, though the request asked for it uncoded: try again" }); kept: request 552 bytes, response 20 bytes; key refused: false
            a stream broken off: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\n, cut short; told: Some(Cut { status: 200, request_id: Some("req_1"), usage: None, failed: None }); kept: request 552 bytes, response 59 bytes; key refused: false
            a hang-up: the stand-in got the same head with the body as sent; the sandbox heard 502 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider gave no answer: try again","param":null,"type":"server_error"}}; told: Some(Refused { upstream_status: None, request_id: None, status: 502, message: "the model provider gave no answer: try again" }); kept: request 552 bytes, response 115 bytes; key refused: false
            plain http beyond the loopback: the upstream's URL is plain http to a host beyond the loopback: name an https URL without a user, or an http URL to a loopback IP address such as 127.0.0.1
            a user: the upstream's URL names a user: name an https URL without a user, or an http URL to a loopback IP address such as 127.0.0.1
            a port beyond 65535: the upstream's URL names a port that is not a number up to 65535: name one, or none for the scheme's default
            another scheme: the upstream's URL is not an http URL: name an https URL without a user, or an http URL to a loopback IP address such as 127.0.0.1
            not a URL: the upstream's URL is not a URL (invalid uri character): name an https URL without a user, or an http URL to a loopback IP address such as 127.0.0.1
            no key file: the key file <dir>/key-14 can't be read (No such file or directory (os error 2)): name a file that holds the model provider's key
            an empty key file: the key file <dir>/key-15 holds no API key: write the key alone, on one line
            a key with a space: the key file <dir>/key-16 holds no API key: write the key alone, on one line
            a stream of Chat Completions: the stand-in got POST /v1/chat/completions HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "355"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "application/json"), ("user-agent", "OpenAI/Python 1.99.0"), ("authorization", "Bearer sk-test-0123")] with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] data: {"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"7"}}],"usage":null}\n\ndata: {"object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":20,"prompt_tokens_details":{"cached_tokens":16},"completion_tokens":1,"completion_tokens_details":{"reasoning_tokens":0}}}\n\ndata: [DONE]\n\n; told: Some(Forwarded { status: 200, request_id: Some("req_1"), usage: Some(Usage { input_tokens: 20, output_tokens: 1, cached_tokens: Some(16), reasoning_tokens: Some(0) }), failed: None }); kept: request 355 bytes, response 319 bytes, its output begun by byte 103; key refused: false
            a rate limit with a 200: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.failed\ndata: {"type":"response.failed","sequence_number":0,"response":{"id":"resp_1","object":"response","status":"failed","error":{"code":"rate_limit_exceeded","message":"Rate limit reached"},"output":[],"usage":null}}\n\nevent: error\ndata: {"type":"error","code":"rate_limit_exceeded","message":"Rate limit reached","param":null,"sequence_number":1}\n\n; told: Some(Forwarded { status: 200, request_id: Some("req_1"), usage: None, failed: Some(Failure { code: Some("rate_limit_exceeded") }) }); kept: request 552 bytes, response 367 bytes; key refused: false
            a failure after output: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\nevent: response.output_text.delta\ndata: {"type":"response.output_text.delta","delta":"Hel"}\n\nevent: response.failed\ndata: {"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"The model failed"},"usage":{"input_tokens":12,"output_tokens":1}}}\n\n; told: Some(Forwarded { status: 200, request_id: Some("req_1"), usage: Some(Usage { input_tokens: 12, output_tokens: 1, cached_tokens: None, reasoning_tokens: None }), failed: Some(Failure { code: Some("server_error") }) }); kept: request 552 bytes, response 347 bytes, its output begun by byte 152; key refused: false
            a failure without a code: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\nevent: response.failed\ndata: {"type":"response.failed","response":{"status":"failed","error":null}}\n\n; told: Some(Forwarded { status: 200, request_id: Some("req_1"), usage: None, failed: Some(Failure { code: None }) }); kept: request 552 bytes, response 160 bytes; key refused: false
            a code that is no identifier: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\nevent: response.failed\ndata: {"type":"response.failed","response":{"status":"failed","error":{"code":"Slow down: wait 3s","message":""}}}\n\n; told: Some(Forwarded { status: 200, request_id: Some("req_1"), usage: None, failed: Some(Failure { code: None }) }); kept: request 552 bytes, response 198 bytes; key refused: false
            an error event: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\nevent: error\ndata: {"type":"error","code":"server_error","message":"An error occurred","param":null}\n\n; told: Some(Forwarded { status: 200, request_id: Some("req_1"), usage: None, failed: Some(Failure { code: Some("server_error") }) }); kept: request 552 bytes, response 161 bytes; key refused: false
            a function call: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\nevent: response.output_item.added\ndata: {"type":"response.output_item.added","item":{"type":"reasoning"}}\n\nevent: response.output_item.added\ndata: {"type":"response.output_item.added","item":{"type":"function_call","name":"exec"}}\n\nevent: response.function_call_arguments.delta\ndata: {"type":"response.function_call_arguments.delta","delta":"{}"}\n\n; told: Some(Forwarded { status: 200, request_id: Some("req_1"), usage: None, failed: None }); kept: request 552 bytes, response 407 bytes, its output begun by byte 407; key refused: false
            reasoning: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"response":{"id":"resp_1","object":"response","output":[],"status":"in_progress"},"sequence_number":0,"type":"response.created"}\n\nevent: response.output_item.added\ndata: {"item":{"content":[],"id":"rs_1","status":"in_progress","summary":[],"type":"reasoning"},"output_index":0,"sequence_number":1,"type":"response.output_item.added"}\n\nevent: response.reasoning_text.delta\ndata: {"content_index":0,"delta":"","item_id":"rs_1","output_index":0,"sequence_number":2,"type":"response.reasoning_text.delta"}\n\nevent: response.reasoning_text.delta\ndata: {"content_index":0,"delta":"Two","item_id":"rs_1","output_index":0,"sequence_number":3,"type":"response.reasoning_text.delta"}\n\n; told: Some(Forwarded { status: 200, request_id: Some("req_1"), usage: None, failed: None }); kept: request 552 bytes, response 705 bytes, its output begun by byte 705; key refused: false
            a call's arguments: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\nevent: response.function_call_arguments.delta\ndata: {"delta":"{\"","item_id":"fc_1","output_index":0,"sequence_number":1,"type":"response.function_call_arguments.delta"}\n\n; told: Some(Forwarded { status: 200, request_id: Some("req_1"), usage: None, failed: None }); kept: request 552 bytes, response 230 bytes, its output begun by byte 230; key refused: false
            a custom tool call: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\nevent: response.output_item.added\ndata: {"type":"response.output_item.added","item":{"type":"custom_tool_call","id":"ctc_1","call_id":"call_1","name":"apply_patch","input":"","status":"in_progress"}}\n\nevent: response.custom_tool_call_input.delta\ndata: {"type":"response.custom_tool_call_input.delta","item_id":"ctc_1","call_id":"call_1","delta":"*** Begin Patch"}\n\n; told: Some(Forwarded { status: 200, request_id: Some("req_1"), usage: None, failed: None }); kept: request 552 bytes, response 424 bytes, its output begun by byte 260; key refused: false
            a custom tool call's input: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\nevent: response.custom_tool_call_input.delta\ndata: {"type":"response.custom_tool_call_input.delta","item_id":"ctc_1","call_id":"call_1","delta":"*** Begin Patch"}\n\n; told: Some(Forwarded { status: 200, request_id: Some("req_1"), usage: None, failed: None }); kept: request 552 bytes, response 223 bytes, its output begun by byte 223; key refused: false
            a refusal: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\nevent: response.refusal.delta\ndata: {"type":"response.refusal.delta","delta":"No"}\n\n; told: Some(Forwarded { status: 200, request_id: Some("req_1"), usage: None, failed: None }); kept: request 552 bytes, response 143 bytes, its output begun by byte 143; key refused: false
            a tool call of Chat Completions: the stand-in got POST /v1/chat/completions HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "355"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "application/json"), ("user-agent", "OpenAI/Python 1.99.0"), ("authorization", "Bearer sk-test-0123")] with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] data: {"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}\n\ndata: {"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"exec","arguments":""}}]}}]}\n\ndata: [DONE]\n\n; told: Some(Forwarded { status: 200, request_id: Some("req_1"), usage: None, failed: None }); kept: request 355 bytes, response 281 bytes, its output begun by byte 281; key refused: false
            reasoning content of Chat Completions: the stand-in got POST /v1/chat/completions HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "355"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "application/json"), ("user-agent", "OpenAI/Python 1.99.0"), ("authorization", "Bearer sk-test-0123")] with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] data: {"id":"chatcmpl_1","created":1,"object":"chat.completion.chunk","model":"m-large","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"Two"}}]}\n\ndata: [DONE]\n\n; told: Some(Forwarded { status: 200, request_id: Some("req_1"), usage: None, failed: None }); kept: request 355 bytes, response 183 bytes, its output begun by byte 169; key refused: false
            reasoning of Chat Completions: the stand-in got POST /v1/chat/completions HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "355"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "application/json"), ("user-agent", "OpenAI/Python 1.99.0"), ("authorization", "Bearer sk-test-0123")] with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] data: {"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"","reasoning":""}}]}\n\ndata: {"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"reasoning":"Two"}}]}\n\ndata: [DONE]\n\n; told: Some(Forwarded { status: 200, request_id: Some("req_1"), usage: None, failed: None }); kept: request 355 bytes, response 231 bytes, its output begun by byte 231; key refused: false
            a failure of Chat Completions: the stand-in got POST /v1/chat/completions HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "355"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "application/json"), ("user-agent", "OpenAI/Python 1.99.0"), ("authorization", "Bearer sk-test-0123")] with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] data: {"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"7"}}],"usage":null}\n\ndata: {"error":{"message":"Rate limit reached","type":"rate_limit_error","param":null,"code":"rate_limit_exceeded"}}\n\n; told: Some(Forwarded { status: 200, request_id: Some("req_1"), usage: None, failed: Some(Failure { code: Some("rate_limit_exceeded") }) }); kept: request 355 bytes, response 221 bytes, its output begun by byte 103; key refused: false"#]],
    );
}

// The usage told is the second part's, which the route read after the sandbox had hung up.
#[test]
fn sandbox_hangs_up_mid_stream() {
    check(
        &[case("a hang-up within the stream", Answer::Unheard)],
        &expect![[
            r#"a hang-up within the stream: the stand-in got POST /v1/responses HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "552"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "text/event-stream"), ("originator", "codex_exec"), ("user-agent", "codex_exec/0.167.0"), ("session-id", "s-1"), ("thread-id", "t-1"), ("x-codex-beta-features", "remote_compaction_v2"), ("x-codex-window-id", "w-1"), ("x-codex-turn-metadata", "{\"turn_id\":\"u-1\"}"), ("x-openai-internal-codex-responses-lite", "true"), ("authorization", "Bearer sk-test-0123")] with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\n, then hung up; told: Some(Cut { status: 200, request_id: Some("req_1"), usage: Some(Usage { input_tokens: 12, output_tokens: 5, cached_tokens: Some(8), reasoning_tokens: Some(3) }), failed: None }); kept: request 552 bytes, response 271 bytes; key refused: false"#
        ]],
    );
}

#[test]
fn shutdown() {
    check(
        &[Case {
            then: Some(true),
            ..case("a request in flight", Answer::Holds)
        }],
        &expect![[
            r#"a request in flight: the stand-in got POST /v1/responses HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "552"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "text/event-stream"), ("originator", "codex_exec"), ("user-agent", "codex_exec/0.167.0"), ("session-id", "s-1"), ("thread-id", "t-1"), ("x-codex-beta-features", "remote_compaction_v2"), ("x-codex-window-id", "w-1"), ("x-codex-turn-metadata", "{\"turn_id\":\"u-1\"}"), ("x-openai-internal-codex-responses-lite", "true"), ("authorization", "Bearer sk-test-0123")] with the body as sent; the sandbox heard 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model route has ended: make no more model requests","param":null,"type":"invalid_request_error"}}; told: Some(Refused { upstream_status: None, request_id: None, status: 400, message: "the model route has ended: make no more model requests" }); kept: request 552 bytes, response 134 bytes; the sandbox heard 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model route has ended: make no more model requests","param":null,"type":"invalid_request_error"}}; told: Some(Barred { status: 400, message: "the model route has ended: make no more model requests" }); kept: request 552 bytes, response 134 bytes; key refused: false"#
        ]],
    );
}

#[test]
fn first_output_as_read() {
    let streams = [
        ("a stream", Api::Responses, RESPONSES_STREAM),
        ("a stream broken off", Api::Responses, RESPONSES_CUT),
        ("a stream of Chat Completions", Api::Chat, CHAT_STREAM),
        ("a custom call of Chat Completions", Api::Chat, CHAT_CUSTOM),
    ];
    let mut cases = Vec::new();
    let mut read = Vec::new();
    for (name, api, stream) in streams {
        let at = route::read::answer(api, stream.as_bytes())
            .first_output
            .unwrap();
        // Split where the reader says the first output's line ends, and a byte before: the route
        // says its output began by the first part's end only if it reads the same event first.
        for split in [at, at - 1] {
            let (first, rest) = stream.split_at(split);
            let parts = [first.to_owned().leak() as &str, rest.to_owned().leak()];
            let answer = Answer::Streams(parts);
            cases.push(match api {
                Api::Responses => case(name, answer),
                Api::Chat => chat_case(name, answer),
            });
            read.push((name, split, stream.len()));
        }
    }
    let exchanged = exchanged(&cases);
    let mut shown = String::new();
    for ((name, split, bytes), line) in read.iter().zip(exchanged.lines()) {
        let began = line.split("its output begun by byte ").nth(1);
        let began = began.and_then(|began| began.split(';').next()).unwrap();
        writeln!(
            shown,
            "{name}, split at {split} of {bytes}: begun by {began}"
        )
        .unwrap();
    }
    expect![[r#"
        a stream, split at 1275 of 6988: begun by 1275
        a stream, split at 1274 of 6988: begun by 6988
        a stream broken off, split at 1262 of 1685: begun by 1262
        a stream broken off, split at 1261 of 1685: begun by 1685
        a stream of Chat Completions, split at 203 of 1206: begun by 203
        a stream of Chat Completions, split at 202 of 1206: begun by 1206
        a custom call of Chat Completions, split at 226 of 412: begun by 226
        a custom call of Chat Completions, split at 225 of 412: begun by 412
    "#]]
    .assert_eq(&shown);
}
