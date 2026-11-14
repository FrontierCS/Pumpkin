//! The model route's closure, against a fake sandbox that sends model requests over a real
//! session, one at a time. The test is the session's owner: it closes each request, and shows
//! what would go upstream, or the refusal that the sandbox heard.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]

use std::collections::BTreeMap;
use std::io::{BufReader, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::{fs, thread};

use expect_test::{Expect, expect};
use gateway::{Arrival, IncomingModelRequest, Registration, Sockets};
use route::{Keep, Key, Route, Upstream, UpstreamUrl};
use schema::ir::{Api, ModelName, Name};
use schema::sandbox::{FromSandbox, Hello, HelloReply, SOCKET_NAME, SocketDial, ToSandbox, frame};
use serde_json::{Value, json};
use stdx::Deadline;
use test_support::HUNG;
use test_support::http::read_request;

mod upstream;

/// A request as its client sends it: method, path, headers and body.
type Request = (&'static str, String, Vec<(String, String)>, Vec<u8>);

/// Each of `names`, which goes upstream under its own name.
fn models<const N: usize>(names: [&str; N]) -> BTreeMap<ModelName, ModelName> {
    let model = |name: &str| ModelName::try_from(name.to_owned()).unwrap();
    names.map(|name| (model(name), model(name))).into()
}

/// Sends each case's request over a session, as a sandbox of a role whose route allows `m-large`
/// and `m-small` through the Responses API and through Chat Completions, and serves it as the
/// session's owner: it shows what the stand-in model
/// provider received of each that the route admitted, or the refusal that the sandbox heard,
/// and checks them against `expect`.
#[expect(
    clippy::too_many_lines,
    reason = "it runs a session, a stand-in and each case's exchange in one place"
)]
fn check(cases: &[(&str, Request)], expect: &Expect) {
    // Kept if the test fails.
    let dir = tempfile::Builder::new()
        .prefix(&format!("route-{}-", thread::current().name().unwrap()))
        .tempdir()
        .unwrap()
        .keep();
    let stand_in = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = stand_in.local_addr().unwrap().to_string();
    let (receipts, received) = mpsc::channel();
    thread::spawn(move || {
        for connection in stand_in.incoming() {
            let mut connection = connection.unwrap();
            let (line, headers, body) = read_request(&mut BufReader::new(&connection));
            let answer = "HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n";
            connection.write_all(answer.as_bytes()).unwrap();
            // The owner may have finished.
            let _ = receipts.send((line, headers, body));
        }
    });
    let key = dir.join("key");
    fs::write(&key, "sk-test-0123\n").unwrap();
    let entry = |api, path: &str| {
        let url = UpstreamUrl::try_from(format!("http://{address}/v1/{path}")).unwrap();
        let route = Route::new(api, models(["m-large", "m-small"]));
        (route, Upstream::new(&url, &Key::read(&key).unwrap()))
    };
    let entries = [
        entry(Api::Responses, "responses"),
        entry(Api::Chat, "chat/completions"),
    ];
    // Its sockets fit in a socket's address by this path to their directory.
    let (_sockets, registration, socket) = meeting(&dir);
    let requests: Vec<Request> = cases.iter().map(|(_, request)| request.clone()).collect();
    let sandbox = thread::spawn(move || fake_sandbox(&socket, requests, None, None));
    let (sender, arrivals) = mpsc::channel();
    let by = Deadline::after(HUNG);
    let accepted = registration.accept(by, None, true, sender, |arrival| arrival, || false);
    let session = accepted.unwrap();
    let sent: Vec<Option<String>> = cases
        .iter()
        .map(|(_, (_, _, _, sent))| {
            let incoming = next_model_request(&arrivals);
            let mut forwarded = false;
            let by = Deadline::after(HUNG);
            let answered = |_, _| {};
            route::serve(
                &entries,
                incoming,
                true,
                by,
                &Ignored,
                || forwarded = true,
                answered,
            );
            forwarded.then(|| {
                let (line, headers, body) = received.recv_timeout(HUNG).unwrap();
                let body = if &body == sent {
                    "the body as sent".to_owned()
                } else {
                    format!("the body {}", String::from_utf8_lossy(&body))
                };
                format!("went upstream, as {line} with {headers:?} and {body}")
            })
        })
        .collect();
    let heard = sandbox.join().unwrap();
    let lines: Vec<String> = cases
        .iter()
        .zip(sent.into_iter().zip(heard))
        .map(|((name, _), (sent, heard))| format!("{name}: {}", sent.unwrap_or(heard)))
        .collect();
    expect.assert_eq(&lines.join("\n").replace(&address, "<stand-in>"));
    drop(session);
    fs::remove_dir_all(dir).unwrap();
}

/// A keeper of exchanges that keeps nothing.
struct Ignored;

impl Keep for Ignored {
    fn request(&self, _: &[u8], _: Option<&'static str>) {}

    fn response(&self, _: &[u8]) {}
}

/// The next of `arrivals`, which must be a model request that arrives within [`HUNG`].
fn next_model_request(arrivals: &Receiver<Arrival>) -> IncomingModelRequest {
    match arrivals.recv_timeout(HUNG) {
        Ok(Arrival::ModelRequest(incoming)) => incoming,
        arrived => panic!("a model request was to arrive, not {arrived:?}"),
    }
}

/// The sockets of a run's sandboxes, made in `dir`, the registration of the agent's socket, and
/// that socket's path.
fn meeting(dir: &Path) -> (Sockets, Registration, PathBuf) {
    let sockets = Sockets::create(dir.join("sockets")).unwrap();
    let name = Name::try_from("agent".to_owned()).unwrap();
    let (SocketDial { dir }, registration) = sockets.issue(&name).unwrap();
    (sockets, registration, dir.join(SOCKET_NAME))
}

/// Opens a session at `socket` with a hello, then sends each request in turn, waiting for its
/// response's end before the next, and returns what it heard back for each: the status, the
/// headers, the body, and whether it was cut short. It tells `parts`, if any, of each part of a
/// body as it hears it, and hangs up as it hears the first of request `hangs_up`'s, if any.
fn fake_sandbox(
    socket: &Path,
    requests: Vec<Request>,
    parts: Option<&Sender<()>>,
    hangs_up: Option<u32>,
) -> Vec<String> {
    let mut connection = UnixStream::connect(socket).unwrap();
    frame::write(
        &mut connection,
        &Hello {
            grant: None,
            gpus: None,
        },
        &[],
    )
    .unwrap();
    let (reply, _) = frame::read::<HelloReply>(&mut connection).unwrap().unwrap();
    assert_eq!(
        reply,
        HelloReply::Accepted { grant: None },
        "the answer to the hello"
    );
    let mut heard = Vec::new();
    for (request, (method, path, headers, body)) in (0..).zip(requests) {
        let open = FromSandbox::ModelRequest {
            request,
            method: method.to_owned(),
            path,
            headers,
            size: body.len().try_into().unwrap(),
        };
        frame::write(&mut connection, &open, &[]).unwrap();
        for chunk in body.chunks(frame::MAX_BODY_BYTES) {
            let data = FromSandbox::ModelData { request };
            frame::write(&mut connection, &data, chunk).unwrap();
        }
        let mut response = String::new();
        loop {
            match frame::read::<ToSandbox>(&mut connection).unwrap().unwrap() {
                (
                    ToSandbox::ModelResponse {
                        status, headers, ..
                    },
                    _,
                ) => response = format!("{status} {headers:?} "),
                (ToSandbox::ModelData { .. }, bytes) => {
                    response.push_str(&String::from_utf8(bytes).unwrap());
                    if hangs_up == Some(request) {
                        // Closed before `parts` hears of the part, so that the next one finds the
                        // session ended.
                        drop(connection);
                        if let Some(parts) = parts {
                            let _ = parts.send(());
                        }
                        heard.push(format!("{response}, then hung up"));
                        return heard;
                    }
                    if let Some(parts) = parts {
                        // Whoever listens may have gone.
                        let _ = parts.send(());
                    }
                }
                (ToSandbox::ModelEnd { cut, .. }, _) => {
                    if cut {
                        response.push_str(", cut short");
                    }
                    break;
                }
                (other, _) => panic!("the sandbox heard {other:?}"),
            }
        }
        heard.push(response);
    }
    heard
}

/// The request headers of the harness the runs use, with ones that must stay behind.
fn harness_headers() -> Vec<(String, String)> {
    [
        ("host", "127.0.0.1:11235"),
        ("content-type", "application/json"),
        ("accept", "text/event-stream"),
        ("authorization", "Bearer the-sandbox's-own"),
        ("cookie", "session=1"),
        ("openai-project", "proj_1"),
        ("originator", "codex_exec"),
        ("user-agent", "codex_exec/0.167.0"),
        ("session-id", "s-1"),
        ("thread-id", "t-1"),
        ("x-codex-beta-features", "remote_compaction_v2"),
        ("x-codex-window-id", "w-1"),
        ("x-codex-turn-metadata", r#"{"turn_id":"u-1"}"#),
        ("x-codex-routing-hint", "model=m-huge;tier=priority"),
        ("x-openai-internal-codex-responses-lite", "true"),
        ("x-client-request-id", "c-1"),
    ]
    .map(|(name, value)| (name.to_owned(), value.to_owned()))
    .to_vec()
}

/// A request's body as the harness sends it: stored nowhere, with tools in an
/// `additional_tools` item, and an image inline. A function's parameters name a `file_id` and
/// `tools`, which are no file and no tools.
fn harness_body() -> Value {
    let parameters = json!({
        "type": "object",
        "properties": { "file_id": { "type": "string" }, "tools": { "type": "array" } }
    });
    json!({
        "model": "m-large",
        "stream": true,
        "store": false,
        "previous_response_id": null,
        "background": false,
        "input": [
            {
                "type": "additional_tools",
                "tools": [{
                    "type": "namespace",
                    "name": "harness",
                    "tools": [
                        { "type": "custom", "name": "exec" },
                        { "type": "function", "name": "wait", "parameters": parameters }
                    ]
                }]
            },
            {
                "type": "message",
                "role": "user",
                "content": [
                    { "type": "input_text", "text": "hi" },
                    { "type": "input_image", "image_url": "data:image/png;base64,AAAA" }
                ]
            }
        ],
        "tools": [{ "type": "tool_search", "execution": "client" }]
    })
}

/// The harness's request with `edit` applied to its body.
fn post(edit: impl FnOnce(&mut Value)) -> Request {
    let mut body = harness_body();
    edit(&mut body);
    let body = body.to_string().into_bytes();
    ("POST", "/responses".to_owned(), harness_headers(), body)
}

/// The request headers of a client of Chat Completions, with ones that must stay behind.
fn chat_headers() -> Vec<(String, String)> {
    [
        ("host", "127.0.0.1:11235"),
        ("content-type", "application/json"),
        ("accept", "application/json"),
        ("authorization", "Bearer the-sandbox's-own"),
        ("openai-organization", "org-1"),
        ("user-agent", "OpenAI/Python 1.99.0"),
        ("x-stainless-lang", "python"),
    ]
    .map(|(name, value)| (name.to_owned(), value.to_owned()))
    .to_vec()
}

/// A client's request of Chat Completions with `edit` applied to its body: streamed, with its
/// usage, a function its model may call, and an image inline.
fn chat(edit: impl FnOnce(&mut Value)) -> Request {
    let mut body = json!({
        "model": "m-small",
        "stream": true,
        "stream_options": { "include_usage": true },
        "messages": [
            { "role": "system", "content": "Grade the answer." },
            {
                "role": "user",
                "content": [
                    { "type": "text", "text": "hi" },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } }
                ]
            }
        ],
        "tools": [{ "type": "function", "function": { "name": "grade", "parameters": {} } }],
        "tool_choice": "auto"
    });
    edit(&mut body);
    let body = body.to_string().into_bytes();
    let path = "/v1/chat/completions".to_owned();
    ("POST", path, chat_headers(), body)
}

/// The harness's request with `name: value` added to its headers.
fn with_header(name: &str, value: &str) -> Request {
    let (method, path, mut headers, body) = post(|_| {});
    headers.push((name.to_owned(), value.to_owned()));
    (method, path, headers, body)
}

/// The harness's request, by `method` to `path`.
fn at(method: &'static str, path: &str) -> Request {
    let (_, _, headers, body) = post(|_| {});
    (method, path.to_owned(), headers, body)
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn closure() {
    let twice = br#"{"model":"m-small","store":true,"store":false,"input":[]}"#;
    check(
        &[
            ("the harness's", post(|_| {})),
            (
                "stored, then not",
                ("POST", "/responses".to_owned(), Vec::new(), twice.to_vec()),
            ),
            (
                "not JSON",
                ("POST", "/responses".to_owned(), Vec::new(), b"{".to_vec()),
            ),
            (
                "no store",
                post(|body| drop(body.as_object_mut().unwrap().remove("store"))),
            ),
            ("stored", post(|body| body["store"] = json!(true))),
            (
                "a previous response",
                post(|body| body["previous_response_id"] = json!("resp_1")),
            ),
            (
                "a conversation",
                post(|body| body["conversation"] = json!("conv_1")),
            ),
            (
                "a stored prompt",
                post(|body| body["prompt"] = json!({ "id": "pmpt_1" })),
            ),
            (
                "in the background",
                post(|body| body["background"] = json!(true)),
            ),
            (
                "a stored item",
                post(|body| {
                    let item = json!({ "type": "item_reference", "id": "msg_1" });
                    body["input"].as_array_mut().unwrap().push(item);
                }),
            ),
            (
                "an item by its id alone",
                post(|body| {
                    let item = json!({ "id": "msg_1" });
                    body["input"].as_array_mut().unwrap().push(item);
                }),
            ),
            (
                "an item of a null type",
                post(|body| {
                    let item = json!({ "type": null, "id": "msg_1" });
                    body["input"].as_array_mut().unwrap().push(item);
                }),
            ),
            (
                "a stored file",
                post(|body| {
                    body["input"][1]["content"][1] =
                        json!({ "type": "input_image", "file_id": "file-1" });
                }),
            ),
            (
                "a shell call in a stored container",
                post(|body| {
                    let item = json!({
                        "type": "shell_call",
                        "call_id": "call_1",
                        "action": { "commands": ["ls"] },
                        "environment": { "type": "container_reference", "container_id": "cntr_1" }
                    });
                    body["input"].as_array_mut().unwrap().push(item);
                }),
            ),
            (
                "a stored response to compare",
                post(|body| {
                    body["prompt_cache_options"] = json!({ "comparison_response_id": "resp_1" });
                }),
            ),
            (
                "another model",
                post(|body| body["model"] = json!("m-huge")),
            ),
            (
                "a model that extends an allowed one",
                post(|body| body["model"] = json!("m-large-pro")),
            ),
            (
                "an access program",
                post(|body| body["access_programs"] = json!({ "cyber": "daybreak_red" })),
            ),
            (
                "a hosted tool chosen",
                post(|body| body["tool_choice"] = json!({ "type": "web_search_preview" })),
            ),
            (
                "web search",
                post(|body| body["tools"] = json!([{ "type": "web_search" }])),
            ),
            (
                "a hosted tool in a namespace",
                post(|body| {
                    body["input"][0]["tools"][0]["tools"][1] =
                        json!({ "type": "code_interpreter" });
                }),
            ),
            (
                "tool search on the server",
                post(|body| body["tools"][0]["execution"] = json!("server")),
            ),
            (
                "tool search with no execution",
                post(|body| body["tools"][0] = json!({ "type": "tool_search" })),
            ),
            (
                "web search in a tool search's output",
                post(|body| {
                    let item = json!({
                        "type": "tool_search_output",
                        "tools": [{ "type": "web_search" }]
                    });
                    body["input"].as_array_mut().unwrap().push(item);
                }),
            ),
            (
                "an image by URL",
                post(|body| {
                    body["input"][1]["content"][1]["image_url"] =
                        json!("https://example.com/a.png");
                }),
            ),
            (
                "an image by URL after a space",
                post(|body| {
                    body["input"][1]["content"][1]["image_url"] =
                        json!(" https://example.com/a.png");
                }),
            ),
            (
                "a file by URL",
                post(|body| {
                    body["input"][1]["content"][1] =
                        json!({ "type": "input_file", "file_url": "HTTP://example.com/a" });
                }),
            ),
            (
                "audio by URL",
                post(|body| {
                    body["input"][1]["content"][1] =
                        json!({ "type": "input_audio", "audio_url": "https://example.com/a.wav" });
                }),
            ),
            (
                "a header value with a line break",
                with_header("x-codex-window-id", "w-1\r\nopenai-project: proj_2"),
            ),
            (
                "a header value with a NUL",
                with_header("x-codex-window-id", "w-1\0"),
            ),
            ("compaction", at("POST", "/responses/compact")),
            ("a long path", at("POST", &format!("/{}", "a".repeat(300)))),
            ("models", at("GET", "/models")),
            ("another method", at("GET", "/responses")),
        ],
        &expect![[r#"
            the harness's: went upstream, as POST /v1/responses HTTP/1.1 with [("accept-encoding", "identity"), ("content-length", "552"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "text/event-stream"), ("originator", "codex_exec"), ("user-agent", "codex_exec/0.167.0"), ("session-id", "s-1"), ("thread-id", "t-1"), ("x-codex-beta-features", "remote_compaction_v2"), ("x-codex-window-id", "w-1"), ("x-codex-turn-metadata", "{\"turn_id\":\"u-1\"}"), ("x-openai-internal-codex-responses-lite", "true"), ("authorization", "Bearer sk-test-0123")] and the body as sent
            stored, then not: went upstream, as POST /v1/responses HTTP/1.1 with [("accept-encoding", "identity"), ("content-length", "44"), ("user-agent", "ureq/3.5.1"), ("accept", "*/*"), ("host", "<stand-in>"), ("authorization", "Bearer sk-test-0123")] and the body {"input":[],"model":"m-small","store":false}
            not JSON: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the body is not a JSON object (EOF while parsing an object at line 1 column 1): send one","param":null,"type":"invalid_request_error"}}
            no store: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"`store` is not false: set it to false, since the route keeps nothing on the provider's side","param":null,"type":"invalid_request_error"}}
            stored: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"`store` is not false: set it to false, since the route keeps nothing on the provider's side","param":null,"type":"invalid_request_error"}}
            a previous response: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"`previous_response_id` refers to state kept on the provider's side, which the route refuses: send the whole context instead","param":null,"type":"invalid_request_error"}}
            a conversation: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"`conversation` refers to state kept on the provider's side, which the route refuses: send the whole context instead","param":null,"type":"invalid_request_error"}}
            a stored prompt: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"`prompt` refers to state kept on the provider's side, which the route refuses: send the whole context instead","param":null,"type":"invalid_request_error"}}
            in the background: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"`background` refers to state kept on the provider's side, which the route refuses: send the whole context instead","param":null,"type":"invalid_request_error"}}
            a stored item: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"an `input` item is an `item_reference` or names no `type`, so it may refer to an item stored on the provider's side: send each item itself, with its `type`","param":null,"type":"invalid_request_error"}}
            an item by its id alone: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"an `input` item is an `item_reference` or names no `type`, so it may refer to an item stored on the provider's side: send each item itself, with its `type`","param":null,"type":"invalid_request_error"}}
            an item of a null type: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"an `input` item is an `item_reference` or names no `type`, so it may refer to an item stored on the provider's side: send each item itself, with its `type`","param":null,"type":"invalid_request_error"}}
            a stored file: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"`file_id` refers to a file stored on the provider's side, which the route refuses: send the content inline","param":null,"type":"invalid_request_error"}}
            a shell call in a stored container: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"`container_id` refers to a container kept on the provider's side, which the route refuses: run commands through a client-side tool","param":null,"type":"invalid_request_error"}}
            a stored response to compare: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"`comparison_response_id` refers to a response stored on the provider's side, which the route refuses: leave it out","param":null,"type":"invalid_request_error"}}
            another model: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the request names `m-huge`, which the route doesn't allow: name `m-large` or `m-small`","param":null,"type":"invalid_request_error"}}
            a model that extends an allowed one: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the request names `m-large-pro`, which the route doesn't allow: name `m-large` or `m-small`","param":null,"type":"invalid_request_error"}}
            an access program: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"`access_programs` picks a program of the provider's, which the route refuses: leave it out","param":null,"type":"invalid_request_error"}}
            a hosted tool chosen: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the tool choice `web_search_preview` names a tool that runs on the provider's side, which the route refuses: choose `auto`, `none`, `required`, or a client-side tool (function, custom or allowed_tools)","param":null,"type":"invalid_request_error"}}
            web search: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the tool `web_search` runs on the provider's side, which the route refuses: declare client-side tools only (function, custom, namespace, and tool_search with client execution)","param":null,"type":"invalid_request_error"}}
            a hosted tool in a namespace: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the tool `code_interpreter` runs on the provider's side, which the route refuses: declare client-side tools only (function, custom, namespace, and tool_search with client execution)","param":null,"type":"invalid_request_error"}}
            tool search on the server: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the tool `tool_search` runs on the provider's side, which the route refuses: declare client-side tools only (function, custom, namespace, and tool_search with client execution)","param":null,"type":"invalid_request_error"}}
            tool search with no execution: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the tool `tool_search` runs on the provider's side, which the route refuses: declare client-side tools only (function, custom, namespace, and tool_search with client execution)","param":null,"type":"invalid_request_error"}}
            web search in a tool search's output: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the tool `web_search` runs on the provider's side, which the route refuses: declare client-side tools only (function, custom, namespace, and tool_search with client execution)","param":null,"type":"invalid_request_error"}}
            an image by URL: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"`image_url` holds a URL the provider would fetch, which the route refuses: send the content inline, as a `data:` URL","param":null,"type":"invalid_request_error"}}
            an image by URL after a space: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"`image_url` holds a URL the provider would fetch, which the route refuses: send the content inline, as a `data:` URL","param":null,"type":"invalid_request_error"}}
            a file by URL: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"`file_url` holds a URL the provider would fetch, which the route refuses: send the content inline, as a `data:` URL","param":null,"type":"invalid_request_error"}}
            audio by URL: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"`audio_url` holds a URL the provider would fetch, which the route refuses: send the content inline, as a `data:` URL","param":null,"type":"invalid_request_error"}}
            a header value with a line break: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the header `x-codex-window-id` holds a control character: send values without any but tab","param":null,"type":"invalid_request_error"}}
            a header value with a NUL: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the header `x-codex-window-id` holds a control character: send values without any but tab","param":null,"type":"invalid_request_error"}}
            compaction: 404 [("content-type", "application/json")] {"error":{"code":null,"message":"`/responses/compact` is not served: send POST /responses or POST /v1/chat/completions","param":null,"type":"invalid_request_error"}}
            a long path: 404 [("content-type", "application/json")] {"error":{"code":null,"message":"`/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa…` is not served: send POST /responses or POST /v1/chat/completions","param":null,"type":"invalid_request_error"}}
            models: 404 [("content-type", "application/json")] {"error":{"code":null,"message":"`/models` is not served: send POST /responses or POST /v1/chat/completions","param":null,"type":"invalid_request_error"}}
            another method: 405 [("content-type", "application/json"), ("allow", "POST")] {"error":{"code":null,"message":"`GET /responses` is not served: send POST /responses","param":null,"type":"invalid_request_error"}}"#]],
    );
}

#[test]
fn chat_closure() {
    let image = |url: &str| json!({ "type": "image_url", "image_url": { "url": url } });
    check(
        &[
            ("the client's", chat(|_| {})),
            ("not stored", chat(|body| body["store"] = json!(false))),
            ("stored", chat(|body| body["store"] = json!(true))),
            (
                "another model",
                chat(|body| body["model"] = json!("m-huge")),
            ),
            (
                "web search",
                chat(|body| body["web_search_options"] = json!({})),
            ),
            (
                "a hosted tool",
                chat(|body| body["tools"][0] = json!({ "type": "web_search_preview" })),
            ),
            (
                "a hosted tool chosen",
                chat(|body| body["tool_choice"] = json!({ "type": "web_search_preview" })),
            ),
            (
                "an image by URL",
                chat(|body| body["messages"][1]["content"][1] = image("https://example.com/a.png")),
            ),
            (
                "a stored file",
                chat(|body| {
                    body["messages"][1]["content"][1] =
                        json!({ "type": "file", "file": { "file_id": "file-1" } });
                }),
            ),
            ("another method", {
                let (_, path, headers, body) = chat(|_| {});
                ("GET", path, headers, body)
            }),
            ("without /v1", {
                let (method, _, headers, body) = chat(|_| {});
                (method, "/chat/completions".to_owned(), headers, body)
            }),
        ],
        &expect![[r#"
            the client's: went upstream, as POST /v1/chat/completions HTTP/1.1 with [("accept-encoding", "identity"), ("content-length", "355"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "application/json"), ("user-agent", "OpenAI/Python 1.99.0"), ("authorization", "Bearer sk-test-0123")] and the body as sent
            not stored: went upstream, as POST /v1/chat/completions HTTP/1.1 with [("accept-encoding", "identity"), ("content-length", "369"), ("host", "<stand-in>"), ("content-type", "application/json"), ("accept", "application/json"), ("user-agent", "OpenAI/Python 1.99.0"), ("authorization", "Bearer sk-test-0123")] and the body as sent
            stored: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"`store` is not false: leave it out, or set it to false, since the route keeps nothing on the provider's side","param":null,"type":"invalid_request_error"}}
            another model: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the request names `m-huge`, which the route doesn't allow: name `m-large` or `m-small`","param":null,"type":"invalid_request_error"}}
            web search: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"`web_search_options` has the provider search the web, which the route refuses: leave it out","param":null,"type":"invalid_request_error"}}
            a hosted tool: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the tool `web_search_preview` runs on the provider's side, which the route refuses: declare client-side tools only (function or custom)","param":null,"type":"invalid_request_error"}}
            a hosted tool chosen: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"the tool choice `web_search_preview` names a tool that runs on the provider's side, which the route refuses: choose `auto`, `none`, `required`, or a client-side tool (function, custom or allowed_tools)","param":null,"type":"invalid_request_error"}}
            an image by URL: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"`url` holds a URL the provider would fetch, which the route refuses: send the content inline, as a `data:` URL","param":null,"type":"invalid_request_error"}}
            a stored file: 400 [("content-type", "application/json")] {"error":{"code":null,"message":"`file_id` refers to a file stored on the provider's side, which the route refuses: send the content inline","param":null,"type":"invalid_request_error"}}
            another method: 405 [("content-type", "application/json"), ("allow", "POST")] {"error":{"code":null,"message":"`GET /v1/chat/completions` is not served: send POST /v1/chat/completions","param":null,"type":"invalid_request_error"}}
            without /v1: 404 [("content-type", "application/json")] {"error":{"code":null,"message":"`/chat/completions` is not served: send POST /responses or POST /v1/chat/completions","param":null,"type":"invalid_request_error"}}"#]],
    );
}
