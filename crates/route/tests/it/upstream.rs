//! The model upstream, against a stand-in model provider: a local HTTP server that answers each
//! request as its case says. The test is the session's owner: it serves the harness's request
//! from the fake sandbox, and shows what the stand-in received and what the sandbox heard.

mod bridge;
mod bridge_stream;

use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::io::{self, BufReader, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener};
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::{fs, iter, slice, thread};

use bridge::bridge_exchange;
use expect_test::{Expect, expect};
use gateway::{Arrival, IncomingModelRequest};
use route::{Admission, Answer as Told, Apart, Keep, Key, Route, Upstream, UpstreamUrl};
use schema::ir::{Api, ModelBudget};
use stdx::Deadline;
use test_support::http::{read_request, write_chunk};
use test_support::{HUNG, TestDir};

use super::kept::{
    CHAT_ANSWER, CHAT_STREAM, MESSAGES_CUT, MESSAGES_STREAM, RESPONSES_CUT, RESPONSES_STREAM,
};
use super::read::CHAT_CUSTOM;
use super::{
    Request, chat, fake_sandbox, listed, meeting, messages, models, next_model_request, post,
};

const KEY: &str = "sk-test-0123\n";

const STAND_IN: &str = "http://{stand-in}/v1/responses";

// Headers that a harness reads, then ones that stay behind.
const STREAM_HEAD: &str = "content-type: text/event-stream\r\nopenai-model: m-large\r\n\
                           x-request-id: req_1\r\nrequest-id: req_1\r\n\
                           x-codex-primary-used-percent: 5\r\n\
                           openai-organization: org-1\r\nopenai-project: proj_1\r\n\
                           x-ratelimit-remaining-requests: 499\r\nset-cookie: s=1\r\n\
                           transfer-encoding: chunked\r\nconnection: close\r\n\r\n";

type Parts = [&'static str; 2];

const PARTS: Parts = [
    "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
    "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":12,\"input_tokens_details\":{\"cached_tokens\":8},\"output_tokens\":5,\"output_tokens_details\":{\"reasoning_tokens\":3}}}}\n\n",
];

const CHAT_PARTS: Parts = [
    "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"7\"}}],\"usage\":null}\n\n",
    "data: {\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":20,\"prompt_tokens_details\":{\"cached_tokens\":16},\"completion_tokens\":1,\"completion_tokens_details\":{\"reasoning_tokens\":0}}}\n\ndata: [DONE]\n\n",
];

const LIMITED_PARTS: Parts = [
    "event: response.failed\ndata: {\"type\":\"response.failed\",\"sequence_number\":0,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"failed\",\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"Rate limit reached\"},\"output\":[],\"usage\":null}}\n\n",
    "event: error\ndata: {\"type\":\"error\",\"code\":\"rate_limit_exceeded\",\"message\":\"Rate limit reached\",\"param\":null,\"sequence_number\":1}\n\n",
];

const FAILED_LATE_PARTS: Parts = [
    "event: response.created\ndata: {\"type\":\"response.created\"}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n",
    "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"server_error\",\"message\":\"The model failed\"},\"usage\":{\"input_tokens\":12,\"output_tokens\":1}}}\n\n",
];

const UNCODED_PARTS: Parts = [
    PARTS[0],
    "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":null}}\n\n",
];

const MISCODED_PARTS: Parts = [
    PARTS[0],
    "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"Slow down: wait 3s\",\"message\":\"\"}}}\n\n",
];

const ERROR_PARTS: Parts = [
    PARTS[0],
    "event: error\ndata: {\"type\":\"error\",\"code\":\"server_error\",\"message\":\"An error occurred\",\"param\":null}\n\n",
];

const CALL_PARTS: Parts = [
    "event: response.created\ndata: {\"type\":\"response.created\"}\n\nevent: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"reasoning\"}}\n\n",
    "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"name\":\"exec\"}}\n\nevent: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{}\"}\n\n",
];

const CHAT_CALL_PARTS: Parts = [
    "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
    "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"exec\",\"arguments\":\"\"}}]}}]}\n\ndata: [DONE]\n\n",
];

const ARGUMENTS_PARTS: Parts = [
    PARTS[0],
    "event: response.function_call_arguments.delta\ndata: {\"delta\":\"{\\\"\",\"item_id\":\"fc_1\",\"output_index\":0,\"sequence_number\":1,\"type\":\"response.function_call_arguments.delta\"}\n\n",
];

const CUSTOM_CALL_PARTS: Parts = [
    "event: response.created\ndata: {\"type\":\"response.created\"}\n\nevent: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"custom_tool_call\",\"id\":\"ctc_1\",\"call_id\":\"call_1\",\"name\":\"apply_patch\",\"input\":\"\",\"status\":\"in_progress\"}}\n\n",
    "event: response.custom_tool_call_input.delta\ndata: {\"type\":\"response.custom_tool_call_input.delta\",\"item_id\":\"ctc_1\",\"call_id\":\"call_1\",\"delta\":\"*** Begin Patch\"}\n\n",
];

const CUSTOM_INPUT_PARTS: Parts = [PARTS[0], CUSTOM_CALL_PARTS[1]];

const REFUSAL_PARTS: Parts = [
    PARTS[0],
    "event: response.refusal.delta\ndata: {\"type\":\"response.refusal.delta\",\"delta\":\"No\"}\n\n",
];

const CHAT_REASONED_PARTS: Parts = [
    "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\",\"reasoning\":\"\"}}]}\n\n",
    "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"Two\"}}]}\n\ndata: [DONE]\n\n",
];

const MESSAGES_FAILED_PARTS: Parts = [
    MESSAGES_PARTS[0],
    "event: error\ndata: {\"type\": \"error\", \"error\": {\"type\": \"overloaded_error\", \"message\": \"Overloaded\"}}\n\n",
];

const CHAT_FAILED_PARTS: Parts = [
    CHAT_PARTS[0],
    "data: {\"error\":{\"message\":\"Rate limit reached\",\"type\":\"rate_limit_error\",\"param\":null,\"code\":\"rate_limit_exceeded\"}}\n\n",
];

const MESSAGES_PARTS: Parts = [
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"m-large\",\"usage\":{\"input_tokens\":12,\"cache_creation_input_tokens\":300,\"cache_read_input_tokens\":2000,\"output_tokens\":1}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"7\"}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
];

const MESSAGES_RECOUNTED_PARTS: Parts = [
    MESSAGES_PARTS[0],
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":20,\"cache_read_input_tokens\":2100,\"output_tokens\":7,\"output_tokens_details\":{\"thinking_tokens\":4}}}\n\n",
];

const MESSAGE: &str = "{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"7\"}],\"model\":\"m-large\",\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":12,\"cache_creation_input_tokens\":300,\"cache_read_input_tokens\":2000,\"output_tokens\":5,\"output_tokens_details\":{\"thinking_tokens\":3}}}";

const MESSAGES_INPUT_PARTS: Parts = [
    MESSAGES_PARTS[0],
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"\"}}\n\n",
];

struct Case {
    name: &'static str,
    api: Api,
    /// The request that the sandbox sends of `api`.
    request: fn(Api) -> Request,
    url: &'static str,
    /// The text of its key file; `None` for no file.
    key: Option<&'static str>,
    /// How the stand-in answers; `None` if it never receives the request.
    answer: Option<Answer>,
    /// Whether a second request, which the stand-in never receives, is served with the same
    /// upstream: `Some(admission)`.
    then: Option<Admission<'static>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Answer {
    Streams(Parts),
    /// As [`Answer::Streams`], under this `content-type`.
    Labelled(&'static str, Parts),
    Json(&'static str),
    Refuses,
    RefusesKey(u16),
    Redirects,
    Codes,
    /// With the stream's head and first part, then it hangs up.
    BreaksOff,
    /// With a stream in two parts, whose sandbox hangs up as it hears the first: the second, and
    /// the stream's end, come then.
    Unheard,
    HangsUp,
    /// It tells the test that it holds the request, then holds it without an answer until the
    /// route's end hangs up on it.
    Holds,
}

fn spent() -> Admission<'static> {
    const BUDGET: ModelBudget = ModelBudget {
        microusd: NonZeroU64::new(1_500_000).unwrap(),
    };
    Admission::Spent { budget: &BUDGET }
}

fn case(name: &'static str, answer: Answer) -> Case {
    Case {
        name,
        api: Api::Responses,
        request,
        url: STAND_IN,
        key: Some(KEY),
        answer: Some(answer),
        then: None,
    }
}

fn chat_case(name: &'static str, answer: Answer) -> Case {
    Case {
        api: Api::Chat,
        url: "http://{stand-in}/v1/chat/completions",
        ..case(name, answer)
    }
}

fn messages_case(name: &'static str, answer: Answer) -> Case {
    Case {
        api: Api::Messages,
        url: "http://{stand-in}/v1/messages",
        ..case(name, answer)
    }
}

fn unanswered(name: &'static str) -> Case {
    Case {
        answer: None,
        ..case(name, Answer::Streams(PARTS))
    }
}

fn check(cases: &[Case], expect: &Expect) {
    expect.assert_eq(&exchanged(cases));
}

#[expect(
    clippy::too_many_lines,
    reason = "it runs a session, a stand-in and each case's exchange in one place"
)]
fn exchanged(cases: &[Case]) -> String {
    let scratch = TestDir::new();
    let dir = scratch.path();
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
    let (_sockets, registration, socket) = meeting(dir);
    let mut requests = Vec::new();
    let mut hangs_up = None;
    for case in &forwarded {
        if case.answer == Some(Answer::Unheard) {
            hangs_up = Some(u32::try_from(requests.len()).unwrap());
        }
        let count = 1 + usize::from(case.then.is_some());
        requests.extend(iter::repeat_n((case.request)(case.api), count));
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
                        let sent = (case.request)(case.api).3;
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
    shown.replace(&dir.display().to_string(), "<dir>")
}

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
            let served = scope.spawn(move || serve_one(entry, incoming, Admission::Open, by));
            held.recv_timeout(HUNG).unwrap();
            entry.1.shutdown();
            served.join().unwrap()
        })
    } else {
        serve_one(entry, incoming, Admission::Open, by)
    };
    let mut served = vec![first];
    if let Some(admission) = case.then {
        let incoming = next_model_request(arrivals);
        served.push(serve_one(entry, incoming, admission, by));
    }
    served
}

fn serve_one(
    entry: &(Route, Upstream),
    incoming: IncomingModelRequest,
    admission: Admission<'_>,
    by: Deadline,
) -> (Option<Told>, String) {
    let kept = Kept::default();
    let mut told = None;
    let answered = |_, answer| told = Some((answer, kept.sizes()));
    route::serve(
        slice::from_ref(entry),
        incoming,
        |_| admission,
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
    translated_to: Cell<Option<Api>>,
    request: RefCell<Vec<u8>>,
    response: RefCell<Vec<u8>>,
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
    fn upstream(&self, api: Option<Api>, _: &[String]) {
        self.translated_to.set(api);
    }

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
        Api::Messages => messages(|_| {}),
    }
}

fn upstream(case: &Case, key: &Path, address: &str) -> Result<(Route, Upstream), String> {
    if let Some(text) = case.key {
        fs::write(key, text).unwrap();
    }
    let url = UpstreamUrl::try_from(case.url.replace("{stand-in}", address))?;
    let route = Route::new(case.api, models(&["m-large", "m-small"]), &listed(case.api));
    Ok((route, Upstream::new(case.api, &url, &Key::read(key)?)))
}

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
            Answer::Streams(_) | Answer::Labelled(..) | Answer::BreaksOff => {
                let (content_type, parts) = match answer {
                    Answer::Streams(parts) => ("text/event-stream", parts),
                    Answer::Labelled(content_type, parts) => (content_type, parts),
                    _ => ("text/event-stream", PARTS),
                };
                let labelled = STREAM_HEAD.replacen("text/event-stream", content_type, 1);
                write!(connection, "HTTP/1.1 200 OK\r\n{labelled}").unwrap();
                // The rest waits for the sandbox to hear the first part, so a route that holds
                // a stream back fails here.
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
            Answer::Json(body) => {
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nrequest-id: req_1\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                connection.write_all((head + body).as_bytes()).unwrap();
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
                then: Some(Admission::Open),
                ..case("the key refused", Answer::RefusesKey(401))
            },
            Case {
                then: Some(Admission::Ended),
                ..case("the work phase ended", Answer::Refuses)
            },
            Case {
                then: Some(spent()),
                ..case("the entry's budget spent", Answer::Refuses)
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
                "reasoning of Chat Completions",
                Answer::Streams(CHAT_REASONED_PARTS),
            ),
            chat_case(
                "a failure of Chat Completions",
                Answer::Streams(CHAT_FAILED_PARTS),
            ),
            messages_case("a stream of Messages", Answer::Streams(MESSAGES_PARTS)),
            messages_case(
                "a stream of Messages that counts its input again",
                Answer::Streams(MESSAGES_RECOUNTED_PARTS),
            ),
            messages_case(
                "a failure of Messages",
                Answer::Streams(MESSAGES_FAILED_PARTS),
            ),
            messages_case("a message of Messages", Answer::Json(MESSAGE)),
            messages_case("the key refused by Messages", Answer::RefusesKey(401)),
            messages_case(
                "a tool call's input of Messages",
                Answer::Streams(MESSAGES_INPUT_PARTS),
            ),
        ],
        &expect![[r#"
            a stream: the stand-in got POST /v1/responses HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "552"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "text/event-stream"), ("originator", "codex_exec"), ("user-agent", "codex_exec/0.167.0"), ("session-id", "s-1"), ("thread-id", "t-1"), ("x-codex-beta-features", "remote_compaction_v2"), ("x-codex-window-id", "w-1"), ("x-codex-turn-metadata", "{\"turn_id\":\"u-1\"}"), ("x-openai-internal-codex-responses-lite", "true"), ("authorization", "Bearer sk-test-0123")] with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\nevent: response.completed\ndata: {"type":"response.completed","response":{"usage":{"input_tokens":12,"input_tokens_details":{"cached_tokens":8},"output_tokens":5,"output_tokens_details":{"reasoning_tokens":3}}}}\n\n; told: Some(Forwarded { model: ModelName("m-large"), status: 200, request_id: Some("req_1"), usage: Some(Usage { input_tokens: 12, output_tokens: 5, cached_tokens: Some(8), cache_write_tokens: None, reasoning_tokens: Some(3) }), failed: None }); kept: request 552 bytes, response 271 bytes; key refused: false
            the provider's refusal: the stand-in got the same head with the body as sent; the sandbox heard 429 [("content-type", "application/json"), ("retry-after", "2")] {"error":{"message":"slow down","type":"requests"}}; told: Some(Forwarded { model: ModelName("m-large"), status: 429, request_id: None, usage: None, failed: None }); kept: request 552 bytes, response 51 bytes; key refused: false
            the key refused: the stand-in got the same head with the body as sent; the sandbox heard 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider refused the route's key, with a 401, which ends the model route: make no more model requests","param":null,"type":"invalid_request_error"}}; told: Some(Refused { upstream_status: Some(401), request_id: None, status: 400, message: "the model provider refused the route's key, with a 401, which ends the model route: make no more model requests" }); kept: request 552 bytes, response 35 bytes; the sandbox heard 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider refused the route's key, with a 401, which ends the model route: make no more model requests","param":null,"type":"invalid_request_error"}}; told: Some(Barred { bar: Ended, status: 400, message: "the model provider refused the route's key, with a 401, which ends the model route: make no more model requests" }); kept: request 552 bytes, response 191 bytes; key refused: true
            the work phase ended: the stand-in got the same head with the body as sent; the sandbox heard 429 [("content-type", "application/json"), ("retry-after", "2")] {"error":{"message":"slow down","type":"requests"}}; told: Some(Forwarded { model: ModelName("m-large"), status: 429, request_id: None, usage: None, failed: None }); kept: request 552 bytes, response 51 bytes; the sandbox heard 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model route has ended: make no more model requests","param":null,"type":"invalid_request_error"}}; told: Some(Barred { bar: Ended, status: 400, message: "the model route has ended: make no more model requests" }); kept: request 552 bytes, response 134 bytes; key refused: false
            the entry's budget spent: the stand-in got the same head with the body as sent; the sandbox heard 429 [("content-type", "application/json"), ("retry-after", "2")] {"error":{"message":"slow down","type":"requests"}}; told: Some(Forwarded { model: ModelName("m-large"), status: 429, request_id: None, usage: None, failed: None }); kept: request 552 bytes, response 51 bytes; the sandbox heard 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model budget of the role's entry for `m-large`, `m-small`, $1.50 at the configured prices, is exhausted: make no more requests for its models","param":null,"type":"invalid_request_error"}}; told: Some(Barred { bar: Budget, status: 400, message: "the model budget of the role's entry for `m-large`, `m-small`, $1.50 at the configured prices, is exhausted: make no more requests for its models" }); kept: request 552 bytes, response 225 bytes; key refused: false
            a request forbidden: the stand-in got the same head with the body as sent; the sandbox heard 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider refused the request, with a 403: send only requests that it allows","param":null,"type":"invalid_request_error"}}; told: Some(Refused { upstream_status: Some(403), request_id: None, status: 400, message: "the model provider refused the request, with a 403: send only requests that it allows" }); kept: request 552 bytes, response 35 bytes; key refused: false
            a redirect: the stand-in got the same head with the body as sent; the sandbox heard 307 [] ; told: Some(Forwarded { model: ModelName("m-large"), status: 307, request_id: None, usage: None, failed: None }); kept: request 552 bytes, response 0 bytes; key refused: false
            a coded body: the stand-in got the same head with the body as sent; the sandbox heard 502 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider coded its answer, though the request asked for it uncoded: try again","param":null,"type":"server_error"}}; told: Some(Refused { upstream_status: Some(200), request_id: None, status: 502, message: "the model provider coded its answer, though the request asked for it uncoded: try again" }); kept: request 552 bytes, response 20 bytes; key refused: false
            a stream broken off: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\n, cut short; told: Some(Cut { model: ModelName("m-large"), status: 200, request_id: Some("req_1"), usage: None, failed: None }); kept: request 552 bytes, response 59 bytes; key refused: false
            a hang-up: the stand-in got the same head with the body as sent; the sandbox heard 502 [("content-type", "application/json")] {"error":{"code":null,"message":"the model provider gave no answer: try again","param":null,"type":"server_error"}}; told: Some(Refused { upstream_status: None, request_id: None, status: 502, message: "the model provider gave no answer: try again" }); kept: request 552 bytes, response 115 bytes; key refused: false
            plain http beyond the loopback: the upstream's URL is plain http to a host beyond the loopback: name an https URL without a user, or an http URL to a loopback IP address such as 127.0.0.1
            a user: the upstream's URL names a user: name an https URL without a user, or an http URL to a loopback IP address such as 127.0.0.1
            a port beyond 65535: the upstream's URL names a port that is not a number up to 65535: name one, or none for the scheme's default
            another scheme: the upstream's URL is not an http URL: name an https URL without a user, or an http URL to a loopback IP address such as 127.0.0.1
            not a URL: the upstream's URL is not a URL (invalid uri character): name an https URL without a user, or an http URL to a loopback IP address such as 127.0.0.1
            no key file: the key file <dir>/key-15 can't be read (No such file or directory (os error 2)): name a file that holds the model provider's key
            an empty key file: the key file <dir>/key-16 holds no API key: write the key alone, on one line
            a key with a space: the key file <dir>/key-17 holds no API key: write the key alone, on one line
            a stream of Chat Completions: the stand-in got POST /v1/chat/completions HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "355"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "application/json"), ("user-agent", "OpenAI/Python 1.99.0"), ("authorization", "Bearer sk-test-0123")] with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("x-request-id", "req_1")] data: {"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"7"}}],"usage":null}\n\ndata: {"object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":20,"prompt_tokens_details":{"cached_tokens":16},"completion_tokens":1,"completion_tokens_details":{"reasoning_tokens":0}}}\n\ndata: [DONE]\n\n; told: Some(Forwarded { model: ModelName("m-small"), status: 200, request_id: Some("req_1"), usage: Some(Usage { input_tokens: 20, output_tokens: 1, cached_tokens: Some(16), cache_write_tokens: None, reasoning_tokens: Some(0) }), failed: None }); kept: request 355 bytes, response 319 bytes, its output begun by byte 103; key refused: false
            a rate limit with a 200: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.failed\ndata: {"type":"response.failed","sequence_number":0,"response":{"id":"resp_1","object":"response","status":"failed","error":{"code":"rate_limit_exceeded","message":"Rate limit reached"},"output":[],"usage":null}}\n\nevent: error\ndata: {"type":"error","code":"rate_limit_exceeded","message":"Rate limit reached","param":null,"sequence_number":1}\n\n; told: Some(Forwarded { model: ModelName("m-large"), status: 200, request_id: Some("req_1"), usage: None, failed: Some(Failure { code: Some("rate_limit_exceeded") }) }); kept: request 552 bytes, response 367 bytes; key refused: false
            a failure after output: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\nevent: response.output_text.delta\ndata: {"type":"response.output_text.delta","delta":"Hel"}\n\nevent: response.failed\ndata: {"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"The model failed"},"usage":{"input_tokens":12,"output_tokens":1}}}\n\n; told: Some(Forwarded { model: ModelName("m-large"), status: 200, request_id: Some("req_1"), usage: Some(Usage { input_tokens: 12, output_tokens: 1, cached_tokens: None, cache_write_tokens: None, reasoning_tokens: None }), failed: Some(Failure { code: Some("server_error") }) }); kept: request 552 bytes, response 347 bytes, its output begun by byte 152; key refused: false
            a failure without a code: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\nevent: response.failed\ndata: {"type":"response.failed","response":{"status":"failed","error":null}}\n\n; told: Some(Forwarded { model: ModelName("m-large"), status: 200, request_id: Some("req_1"), usage: None, failed: Some(Failure { code: None }) }); kept: request 552 bytes, response 160 bytes; key refused: false
            a code that is no identifier: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\nevent: response.failed\ndata: {"type":"response.failed","response":{"status":"failed","error":{"code":"Slow down: wait 3s","message":""}}}\n\n; told: Some(Forwarded { model: ModelName("m-large"), status: 200, request_id: Some("req_1"), usage: None, failed: Some(Failure { code: None }) }); kept: request 552 bytes, response 198 bytes; key refused: false
            an error event: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\nevent: error\ndata: {"type":"error","code":"server_error","message":"An error occurred","param":null}\n\n; told: Some(Forwarded { model: ModelName("m-large"), status: 200, request_id: Some("req_1"), usage: None, failed: Some(Failure { code: Some("server_error") }) }); kept: request 552 bytes, response 161 bytes; key refused: false
            a function call: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\nevent: response.output_item.added\ndata: {"type":"response.output_item.added","item":{"type":"reasoning"}}\n\nevent: response.output_item.added\ndata: {"type":"response.output_item.added","item":{"type":"function_call","name":"exec"}}\n\nevent: response.function_call_arguments.delta\ndata: {"type":"response.function_call_arguments.delta","delta":"{}"}\n\n; told: Some(Forwarded { model: ModelName("m-large"), status: 200, request_id: Some("req_1"), usage: None, failed: None }); kept: request 552 bytes, response 407 bytes, its output begun by byte 407; key refused: false
            a call's arguments: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\nevent: response.function_call_arguments.delta\ndata: {"delta":"{\"","item_id":"fc_1","output_index":0,"sequence_number":1,"type":"response.function_call_arguments.delta"}\n\n; told: Some(Forwarded { model: ModelName("m-large"), status: 200, request_id: Some("req_1"), usage: None, failed: None }); kept: request 552 bytes, response 230 bytes, its output begun by byte 230; key refused: false
            a custom tool call: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\nevent: response.output_item.added\ndata: {"type":"response.output_item.added","item":{"type":"custom_tool_call","id":"ctc_1","call_id":"call_1","name":"apply_patch","input":"","status":"in_progress"}}\n\nevent: response.custom_tool_call_input.delta\ndata: {"type":"response.custom_tool_call_input.delta","item_id":"ctc_1","call_id":"call_1","delta":"*** Begin Patch"}\n\n; told: Some(Forwarded { model: ModelName("m-large"), status: 200, request_id: Some("req_1"), usage: None, failed: None }); kept: request 552 bytes, response 424 bytes, its output begun by byte 260; key refused: false
            a custom tool call's input: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\nevent: response.custom_tool_call_input.delta\ndata: {"type":"response.custom_tool_call_input.delta","item_id":"ctc_1","call_id":"call_1","delta":"*** Begin Patch"}\n\n; told: Some(Forwarded { model: ModelName("m-large"), status: 200, request_id: Some("req_1"), usage: None, failed: None }); kept: request 552 bytes, response 223 bytes, its output begun by byte 223; key refused: false
            a refusal: the stand-in got the same head with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\nevent: response.refusal.delta\ndata: {"type":"response.refusal.delta","delta":"No"}\n\n; told: Some(Forwarded { model: ModelName("m-large"), status: 200, request_id: Some("req_1"), usage: None, failed: None }); kept: request 552 bytes, response 143 bytes, its output begun by byte 143; key refused: false
            a tool call of Chat Completions: the stand-in got POST /v1/chat/completions HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "355"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "application/json"), ("user-agent", "OpenAI/Python 1.99.0"), ("authorization", "Bearer sk-test-0123")] with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("x-request-id", "req_1")] data: {"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}\n\ndata: {"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"exec","arguments":""}}]}}]}\n\ndata: [DONE]\n\n; told: Some(Forwarded { model: ModelName("m-small"), status: 200, request_id: Some("req_1"), usage: None, failed: None }); kept: request 355 bytes, response 281 bytes, its output begun by byte 281; key refused: false
            reasoning of Chat Completions: the stand-in got POST /v1/chat/completions HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "355"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "application/json"), ("user-agent", "OpenAI/Python 1.99.0"), ("authorization", "Bearer sk-test-0123")] with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("x-request-id", "req_1")] data: {"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"","reasoning":""}}]}\n\ndata: {"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"reasoning":"Two"}}]}\n\ndata: [DONE]\n\n; told: Some(Forwarded { model: ModelName("m-small"), status: 200, request_id: Some("req_1"), usage: None, failed: None }); kept: request 355 bytes, response 231 bytes, its output begun by byte 231; key refused: false
            a failure of Chat Completions: the stand-in got POST /v1/chat/completions HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "355"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "application/json"), ("user-agent", "OpenAI/Python 1.99.0"), ("authorization", "Bearer sk-test-0123")] with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("x-request-id", "req_1")] data: {"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"7"}}],"usage":null}\n\ndata: {"error":{"message":"Rate limit reached","type":"rate_limit_error","param":null,"code":"rate_limit_exceeded"}}\n\n; told: Some(Forwarded { model: ModelName("m-small"), status: 200, request_id: Some("req_1"), usage: None, failed: Some(Failure { code: Some("rate_limit_exceeded") }) }); kept: request 355 bytes, response 221 bytes, its output begun by byte 103; key refused: false
            a stream of Messages: the stand-in got POST /v1/messages HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "813"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "application/json"), ("anthropic-version", "2023-06-01"), ("anthropic-beta", "interleaved-thinking-2025-05-14"), ("user-agent", "claude-cli/2.1.311 (external, cli)"), ("x-app", "cli"), ("x-api-key", "sk-test-0123")] with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("request-id", "req_1")] event: message_start\ndata: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"m-large","usage":{"input_tokens":12,"cache_creation_input_tokens":300,"cache_read_input_tokens":2000,"output_tokens":1}}}\n\nevent: content_block_start\ndata: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}\n\nevent: content_block_delta\ndata: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"7"}}\n\nevent: message_delta\ndata: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}\n\nevent: message_stop\ndata: {"type":"message_stop"}\n\n; told: Some(Forwarded { model: ModelName("m-large"), status: 200, request_id: Some("req_1"), usage: Some(Usage { input_tokens: 2312, output_tokens: 5, cached_tokens: Some(2000), cache_write_tokens: Some(300), reasoning_tokens: None }), failed: None }); kept: request 813 bytes, response 656 bytes, its output begun by byte 656; key refused: false
            a stream of Messages that counts its input again: the stand-in got POST /v1/messages HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "813"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "application/json"), ("anthropic-version", "2023-06-01"), ("anthropic-beta", "interleaved-thinking-2025-05-14"), ("user-agent", "claude-cli/2.1.311 (external, cli)"), ("x-app", "cli"), ("x-api-key", "sk-test-0123")] with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("request-id", "req_1")] event: message_start\ndata: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"m-large","usage":{"input_tokens":12,"cache_creation_input_tokens":300,"cache_read_input_tokens":2000,"output_tokens":1}}}\n\nevent: message_delta\ndata: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":20,"cache_read_input_tokens":2100,"output_tokens":7,"output_tokens_details":{"thinking_tokens":4}}}\n\n; told: Some(Forwarded { model: ModelName("m-large"), status: 200, request_id: Some("req_1"), usage: Some(Usage { input_tokens: 2420, output_tokens: 7, cached_tokens: Some(2100), cache_write_tokens: Some(300), reasoning_tokens: Some(4) }), failed: None }); kept: request 813 bytes, response 467 bytes; key refused: false
            a failure of Messages: the stand-in got POST /v1/messages HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "813"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "application/json"), ("anthropic-version", "2023-06-01"), ("anthropic-beta", "interleaved-thinking-2025-05-14"), ("user-agent", "claude-cli/2.1.311 (external, cli)"), ("x-app", "cli"), ("x-api-key", "sk-test-0123")] with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("request-id", "req_1")] event: message_start\ndata: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"m-large","usage":{"input_tokens":12,"cache_creation_input_tokens":300,"cache_read_input_tokens":2000,"output_tokens":1}}}\n\nevent: error\ndata: {"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}\n\n; told: Some(Forwarded { model: ModelName("m-large"), status: 200, request_id: Some("req_1"), usage: Some(Usage { input_tokens: 2312, output_tokens: 1, cached_tokens: Some(2000), cache_write_tokens: Some(300), reasoning_tokens: None }), failed: Some(Failure { code: Some("overloaded_error") }) }); kept: request 813 bytes, response 358 bytes; key refused: false
            a message of Messages: the stand-in got POST /v1/messages HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "813"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "application/json"), ("anthropic-version", "2023-06-01"), ("anthropic-beta", "interleaved-thinking-2025-05-14"), ("user-agent", "claude-cli/2.1.311 (external, cli)"), ("x-app", "cli"), ("x-api-key", "sk-test-0123")] with the body as sent; the sandbox heard 200 [("content-type", "application/json"), ("request-id", "req_1")] {"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"7"}],"model":"m-large","stop_reason":"end_turn","usage":{"input_tokens":12,"cache_creation_input_tokens":300,"cache_read_input_tokens":2000,"output_tokens":5,"output_tokens_details":{"thinking_tokens":3}}}; told: Some(Forwarded { model: ModelName("m-large"), status: 200, request_id: Some("req_1"), usage: Some(Usage { input_tokens: 2312, output_tokens: 5, cached_tokens: Some(2000), cache_write_tokens: Some(300), reasoning_tokens: Some(3) }), failed: None }); kept: request 813 bytes, response 289 bytes; key refused: false
            the key refused by Messages: the stand-in got POST /v1/messages HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "813"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "application/json"), ("anthropic-version", "2023-06-01"), ("anthropic-beta", "interleaved-thinking-2025-05-14"), ("user-agent", "claude-cli/2.1.311 (external, cli)"), ("x-app", "cli"), ("x-api-key", "sk-test-0123")] with the body as sent; the sandbox heard 400 [("content-type", "application/json")] {"error":{"message":"the model provider refused the route's key, with a 401, which ends the model route: make no more model requests","type":"invalid_request_error"},"type":"error"}; told: Some(Refused { upstream_status: Some(401), request_id: None, status: 400, message: "the model provider refused the route's key, with a 401, which ends the model route: make no more model requests" }); kept: request 813 bytes, response 35 bytes; key refused: true
            a tool call's input of Messages: the stand-in got POST /v1/messages HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "813"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "application/json"), ("anthropic-version", "2023-06-01"), ("anthropic-beta", "interleaved-thinking-2025-05-14"), ("user-agent", "claude-cli/2.1.311 (external, cli)"), ("x-app", "cli"), ("x-api-key", "sk-test-0123")] with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("request-id", "req_1")] event: message_start\ndata: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"m-large","usage":{"input_tokens":12,"cache_creation_input_tokens":300,"cache_read_input_tokens":2000,"output_tokens":1}}}\n\nevent: content_block_delta\ndata: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\""}}\n\n; told: Some(Forwarded { model: ModelName("m-large"), status: 200, request_id: Some("req_1"), usage: Some(Usage { input_tokens: 2312, output_tokens: 1, cached_tokens: Some(2000), cache_write_tokens: Some(300), reasoning_tokens: None }), failed: None }); kept: request 813 bytes, response 388 bytes, its output begun by byte 388; key refused: false"#]],
    );
}

// The usage told is the second part's, which the route read after the sandbox had hung up.
#[test]
fn sandbox_hangs_up_mid_stream() {
    check(
        &[case("a hang-up within the stream", Answer::Unheard)],
        &expect![[
            r#"a hang-up within the stream: the stand-in got POST /v1/responses HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "552"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "text/event-stream"), ("originator", "codex_exec"), ("user-agent", "codex_exec/0.167.0"), ("session-id", "s-1"), ("thread-id", "t-1"), ("x-codex-beta-features", "remote_compaction_v2"), ("x-codex-window-id", "w-1"), ("x-codex-turn-metadata", "{\"turn_id\":\"u-1\"}"), ("x-openai-internal-codex-responses-lite", "true"), ("authorization", "Bearer sk-test-0123")] with the body as sent; the sandbox heard 200 [("content-type", "text/event-stream"), ("openai-model", "m-large"), ("x-request-id", "req_1"), ("x-codex-primary-used-percent", "5")] event: response.created\ndata: {"type":"response.created"}\n\n, then hung up; told: Some(Cut { model: ModelName("m-large"), status: 200, request_id: Some("req_1"), usage: Some(Usage { input_tokens: 12, output_tokens: 5, cached_tokens: Some(8), cache_write_tokens: None, reasoning_tokens: Some(3) }), failed: None }); kept: request 552 bytes, response 271 bytes; key refused: false"#
        ]],
    );
}

#[test]
fn shutdown() {
    check(
        &[Case {
            then: Some(Admission::Open),
            ..case("a request in flight", Answer::Holds)
        }],
        &expect![[
            r#"a request in flight: the stand-in got POST /v1/responses HTTP/1.1 [("accept-encoding", "identity"), ("content-length", "552"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "text/event-stream"), ("originator", "codex_exec"), ("user-agent", "codex_exec/0.167.0"), ("session-id", "s-1"), ("thread-id", "t-1"), ("x-codex-beta-features", "remote_compaction_v2"), ("x-codex-window-id", "w-1"), ("x-codex-turn-metadata", "{\"turn_id\":\"u-1\"}"), ("x-openai-internal-codex-responses-lite", "true"), ("authorization", "Bearer sk-test-0123")] with the body as sent; the sandbox heard 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model route has ended: make no more model requests","param":null,"type":"invalid_request_error"}}; told: Some(Abandoned { model: ModelName("m-large") }); kept: request 552 bytes, response 134 bytes; the sandbox heard 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the model route has ended: make no more model requests","param":null,"type":"invalid_request_error"}}; told: Some(Barred { bar: Ended, status: 400, message: "the model route has ended: make no more model requests" }); kept: request 552 bytes, response 134 bytes; key refused: false"#
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
        ("a stream of Messages", Api::Messages, MESSAGES_STREAM),
        (
            "a stream of Messages broken off",
            Api::Messages,
            MESSAGES_CUT,
        ),
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
                Api::Messages => messages_case(name, answer),
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
        a stream of Messages, split at 604 of 2207: begun by 604
        a stream of Messages, split at 603 of 2207: begun by 2207
        a stream of Messages broken off, split at 385 of 525: begun by 385
        a stream of Messages broken off, split at 384 of 525: begun by 525
    "#]]
    .assert_eq(&shown);
}

// Bodies kept from real upstreams, each under the `content-type` of the other framing, as a proxy
// may label them.
#[test]
fn mislabelled() {
    let bodies = [
        (
            "a stream",
            Api::Responses,
            "application/json",
            RESPONSES_STREAM,
        ),
        (
            "a stream of Chat Completions",
            Api::Chat,
            "application/json",
            CHAT_STREAM,
        ),
        (
            "an answer of Chat Completions",
            Api::Chat,
            "text/event-stream",
            CHAT_ANSWER,
        ),
    ];
    let cases = bodies.map(|(name, api, content_type, body)| {
        let (first, rest) = body.split_at(body.floor_char_boundary(body.len() / 2));
        let answer = Answer::Labelled(content_type, [first, rest]);
        match api {
            Api::Responses => case(name, answer),
            Api::Chat => chat_case(name, answer),
            Api::Messages => messages_case(name, answer),
        }
    });
    let exchanged = exchanged(&cases);
    let mut shown = String::new();
    for ((name, _, content_type, _), line) in bodies.iter().zip(exchanged.lines()) {
        let told = line.split("; told: ").nth(1).unwrap();
        writeln!(shown, "{name} as {content_type}: {told}").unwrap();
    }
    expect![[r#"
        a stream as application/json: Some(Forwarded { model: ModelName("m-large"), status: 200, request_id: Some("req_1"), usage: Some(Usage { input_tokens: 102967, output_tokens: 88, cached_tokens: Some(102720), cache_write_tokens: None, reasoning_tokens: Some(4) }), failed: None }); kept: request 552 bytes, response 6988 bytes, its output begun by byte 3494; key refused: false
        a stream of Chat Completions as application/json: Some(Forwarded { model: ModelName("m-small"), status: 200, request_id: Some("req_1"), usage: Some(Usage { input_tokens: 20, output_tokens: 57, cached_tokens: Some(0), cache_write_tokens: None, reasoning_tokens: Some(52) }), failed: None }); kept: request 355 bytes, response 1206 bytes, its output begun by byte 603; key refused: false
        an answer of Chat Completions as text/event-stream: Some(Forwarded { model: ModelName("m-small"), status: 200, request_id: Some("req_1"), usage: Some(Usage { input_tokens: 3323, output_tokens: 57, cached_tokens: Some(0), cache_write_tokens: None, reasoning_tokens: Some(34) }), failed: None }); kept: request 355 bytes, response 798 bytes; key refused: false
    "#]].assert_eq(&shown);
}
