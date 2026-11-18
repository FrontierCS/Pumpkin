//! Responses callers of native and Chat models in the same route and archive.

use std::net::{Ipv4Addr, TcpListener};
use std::sync::mpsc;
use std::{fs, thread};

use expect_test::{Expect, expect};
use route::{Admission, Answer as Told, Key, Route, Upstream, UpstreamUrl};
use schema::ir::{Api, ModelName};
use serde_json::{Value, json};
use stdx::Deadline;
use test_support::{HUNG, TestDir};

use super::super::{fake_sandbox, listed, meeting, models, next_model_request, post};
use super::{Answer, KEY, Kept, serve};

pub(super) fn bridge_exchange(
    input: Value,
    answer: Option<Answer>,
) -> (String, Option<Told>, Vec<u8>, Vec<u8>) {
    let scratch = TestDir::new();
    let dir = scratch.path();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let key = dir.join("key");
    fs::write(&key, KEY).unwrap();
    let mut allowed = models(&["m-small", "m-large"]);
    let small = ModelName::try_from("m-small".to_owned()).unwrap();
    let model = allowed.get_mut(&small).unwrap();
    model.name = ModelName::try_from("vendor/small".to_owned()).unwrap();
    let endpoint = |path| UpstreamUrl::try_from(format!("http://{address}/v1/{path}")).unwrap();
    model.responses_via_chat = true;
    let entry = (
        Route::new(Api::Responses, allowed, &listed(Api::Responses)),
        Upstream::new(
            Api::Responses,
            &endpoint("responses"),
            &Key::read(&key).unwrap(),
        ),
    );
    let (sockets, registration, socket) = meeting(dir);
    let (parts, heard_part) = mpsc::channel();
    let (holds, _) = mpsc::channel();
    let server =
        thread::spawn(move || serve(&listener, answer.into_iter().collect(), &heard_part, &holds));
    let translated = input["model"] == "m-small";
    let incoming = post(|body| *body = input);
    let sandbox = thread::spawn(move || fake_sandbox(&socket, vec![incoming], Some(&parts), None));
    let (sender, arrivals) = mpsc::channel();
    let by = Deadline::after(HUNG);
    let session = registration
        .accept(by, None, true, sender, |arrival| arrival, || false)
        .unwrap();
    let kept = Kept::default();
    let mut told = None;
    let mut forwards = 0;
    route::serve(
        &[entry],
        next_model_request(&arrivals),
        |_| Admission::Open,
        by,
        &kept,
        || forwards += 1,
        |index, answer| {
            assert_eq!(index, Some(0));
            told = Some(answer);
        },
    );
    assert_eq!(forwards, usize::from(answer.is_some()));
    let heard = sandbox.join().unwrap().pop().unwrap();
    let received = server.join().unwrap();
    check_archive(answer, &received, &kept, translated);
    drop(session);
    drop(sockets);
    (
        heard,
        told,
        kept.request.into_inner(),
        kept.response.into_inner(),
    )
}

fn check_archive(
    answer: Option<Answer>,
    received: &[(String, Vec<u8>)],
    kept: &Kept,
    translated: bool,
) {
    if let Some(answer) = answer {
        let [(request_line, body)] = received else {
            panic!("expected one upstream request")
        };
        assert_eq!(body, &*kept.request.borrow());
        let path = if translated {
            "/v1/chat/completions"
        } else {
            "/v1/responses"
        };
        assert!(request_line.starts_with(&format!("POST {path} ")));
        assert_eq!(kept.translated_to.get(), translated.then_some(Api::Chat));
        let raw = match answer {
            Answer::Json(body) => body.to_owned(),
            Answer::Streams(parts) => parts.concat(),
            Answer::ContextExceeded => r#"{"error":{"code":"context_length_exceeded","message":"private provider diagnostics","type":"invalid_request_error","param":null}}"#.to_owned(),
            Answer::Refuses => r#"{"error":{"message":"slow down","type":"requests"}}"#.to_owned(),
            _ => panic!("unexpected fixture"),
        };
        assert_eq!(*kept.response.borrow(), raw.as_bytes());
    } else {
        assert_eq!(received.len(), 0);
    }
}

const CALL: &str = r#"{"object":"chat.completion","id":"completion1","model":"vendor/small","created":1730000000,"choices":[{"index":0,"finish_reason":"tool_calls","message":{"role":"assistant","content":"Counting.","tool_calls":[{"type":"function","id":"call_a","function":{"name":"count","arguments":"{}"}},{"type":"function","id":"call_b","function":{"name":"count","arguments":"{\"word\":\"pear\"}"}}]}}],"usage":{"prompt_tokens":11,"completion_tokens":6,"total_tokens":17,"completion_tokens_details":{"reasoning_tokens":0}}}"#;
const TEXT: &str = r#"{"object":"chat.completion","id":"completion2","model":"vendor/small","created":1730000001,"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"Three."}}]}"#;
const LIMITED: &str = r#"{"object":"chat.completion","id":"completion3","model":"vendor/small","created":1730000003,"choices":[{"index":0,"finish_reason":"length","message":{"role":"assistant","content":"Thr"}}]}"#;

fn stream_replayed() -> String {
    let first = concat!(
        "data: ",
        r#"{"object":"chat.completion.chunk","id":"c","created":123,"model":"vendor/small","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"first","type":"function","function":{"name":"count","arguments":"{}"}}]}}]}"#,
        "\n\n",
    );
    let rest = concat!(
        "data: ",
        r#"{"object":"chat.completion.chunk","id":"c","created":123,"model":"vendor/small","choices":[{"index":0,"delta":{"content":"One more."}}]}"#,
        "\n\n",
        "data: ",
        r#"{"object":"chat.completion.chunk","id":"c","created":123,"model":"vendor/small","choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"second","type":"function","function":{"name":"count","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
        "\n\ndata: [DONE]\n\n",
    );
    let mut request = json!({"model":"m-small","store":false,"stream":true,"input":"How many?"});
    let heard = bridge_exchange(request.clone(), Some(Answer::Streams([first, rest]))).0;
    let done: Value = serde_json::from_str(
        heard
            .trim_end()
            .lines()
            .last()
            .unwrap()
            .strip_prefix("data: ")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(done["type"], "response.completed");
    let mut items = vec![json!({"role":"user","content":"How many?"})];
    items.extend(
        done["response"]["output"]
            .as_array()
            .unwrap()
            .iter()
            .cloned(),
    );
    items.extend(
        ["second", "first"]
            .map(|id| json!({"type":"function_call_output","call_id":id,"output":"3"})),
    );
    request["input"] = json!(items);
    request["stream"] = json!(false);
    let reply = bridge_exchange(request, Some(Answer::Json(TEXT)));
    let chat: Value = serde_json::from_slice(&reply.2).unwrap();
    format!(
        "replayed {} as {}",
        done["response"]["output"], chat["messages"]
    )
}

fn check(expect: &Expect) {
    let mut request = json!({
        "model":"m-small","store":false,"instructions":"Count words.",
        "input":"Words?","max_output_tokens":100,
        "tools":[{"type":"function","name":"count","strict":false,"parameters":{"type":"object"}}],
        "tool_choice":{"type":"function","name":"count"},
        "include":["reasoning.encrypted_content"], "reasoning":{"effort":"medium"},
        "prompt_cache_key":"session1", "client_metadata":{"session_id":"session1","thread_id":"thread1",
        "x-codex-installation-id":"install1","x-codex-window-id":"window1","x-codex-turn-metadata":"{}"}
    });
    let mut shown = Vec::new();
    let mut turn = |request: &Value, reply| {
        let (heard, told, archived, _) =
            bridge_exchange(request.clone(), Some(Answer::Json(reply)));
        let (_, body) = heard.split_once("] ").unwrap();
        let body: Value = serde_json::from_str(body).unwrap();
        let archived: Value = serde_json::from_slice(&archived).unwrap();
        assert_eq!(body["model"], request["model"]);
        assert_eq!(body["tools"], request["tools"]);
        shown.push(format!(
            "{archived}\n{} {} {} {}\n{told:?}",
            body["status"], body["incomplete_details"], body["output"], body["usage"]
        ));
        body
    };
    let first = turn(&request, CALL);
    let mut history = vec![json!({"role":"user","content":"Words?"})];
    history.extend(first["output"].as_array().unwrap().iter().cloned());
    history.extend(
        ["call_b", "call_a"]
            .map(|id| json!({"type":"function_call_output","call_id":id,"output":"3"})),
    );
    request["input"] = Value::Array(history);
    turn(&request, TEXT);
    turn(&request, LIMITED);
    let error = bridge_exchange(request.clone(), Some(Answer::Refuses)).0;
    shown.push(error);
    shown.push(bridge_exchange(request.clone(), Some(Answer::ContextExceeded)).0);
    for edit in [
        json!({"previous_response_id":"stored"}),
        json!({"tools":[{"type":"custom","name":"count"}]}),
        json!({"input":[{"type":"message","role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,AAAA"}]}]}),
        json!({"include":["output.encrypted_content"]}),
        json!({"unexpected":true}),
    ] {
        let mut denied = request.clone();
        denied
            .as_object_mut()
            .unwrap()
            .extend(edit.as_object().unwrap().clone());
        let (heard, told, _, _) = bridge_exchange(denied, None);
        assert!(matches!(told, Some(Told::Barred { .. })));
        shown.push(heard);
    }
    let native = json!({"id":"resp_native","object":"response","model":"m-large","output":[],"status":"completed"}).to_string();
    let (heard, _, _, _) = bridge_exchange(
        json!({"model":"m-large","input":"Words?","store":false}),
        Some(Answer::Json(NATIVE)),
    );
    assert!(heard.ends_with(&native));
    shown.push(stream_replayed());
    expect.assert_eq(&shown.join("\n"));
}

const NATIVE: &str = r#"{"id":"resp_native","model":"m-large","object":"response","output":[],"status":"completed"}"#;

#[test]
fn function_round_trip() {
    check(&expect![[r#"
        {"max_completion_tokens":100,"messages":[{"content":"Count words.","role":"developer"},{"content":"Words?","role":"user"}],"model":"vendor/small","prompt_cache_key":"session1","reasoning_effort":"medium","store":false,"tool_choice":{"function":{"name":"count"},"type":"function"},"tools":[{"function":{"name":"count","parameters":{"type":"object"},"strict":false},"type":"function"}]}
        "completed" null [{"content":[{"annotations":[],"text":"Counting.","type":"output_text"}],"id":"msg_completion1","role":"assistant","status":"completed","type":"message"},{"arguments":"{}","call_id":"call_a","id":"fc_1_completion1","name":"count","status":"completed","type":"function_call"},{"arguments":"{\"word\":\"pear\"}","call_id":"call_b","id":"fc_2_completion1","name":"count","status":"completed","type":"function_call"}] {"input_tokens":11,"input_tokens_details":{"cache_write_tokens":0,"cached_tokens":0},"output_tokens":6,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":17}
        Some(Forwarded { model: ModelName("m-small"), status: 200, request_id: Some("req_1"), usage: Some(Usage { input_tokens: 11, output_tokens: 6, cached_tokens: None, cache_write_tokens: None, reasoning_tokens: Some(0) }), failed: None })
        {"max_completion_tokens":100,"messages":[{"content":"Count words.","role":"developer"},{"content":"Words?","role":"user"},{"content":[{"text":"Counting.","type":"text"}],"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"count"},"id":"call_a","type":"function"},{"function":{"arguments":"{\"word\":\"pear\"}","name":"count"},"id":"call_b","type":"function"}]},{"content":"3","role":"tool","tool_call_id":"call_b"},{"content":"3","role":"tool","tool_call_id":"call_a"}],"model":"vendor/small","prompt_cache_key":"session1","reasoning_effort":"medium","store":false,"tool_choice":{"function":{"name":"count"},"type":"function"},"tools":[{"function":{"name":"count","parameters":{"type":"object"},"strict":false},"type":"function"}]}
        "completed" null [{"content":[{"annotations":[],"text":"Three.","type":"output_text"}],"id":"msg_completion2","role":"assistant","status":"completed","type":"message"}] null
        Some(Forwarded { model: ModelName("m-small"), status: 200, request_id: Some("req_1"), usage: None, failed: None })
        {"max_completion_tokens":100,"messages":[{"content":"Count words.","role":"developer"},{"content":"Words?","role":"user"},{"content":[{"text":"Counting.","type":"text"}],"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"count"},"id":"call_a","type":"function"},{"function":{"arguments":"{\"word\":\"pear\"}","name":"count"},"id":"call_b","type":"function"}]},{"content":"3","role":"tool","tool_call_id":"call_b"},{"content":"3","role":"tool","tool_call_id":"call_a"}],"model":"vendor/small","prompt_cache_key":"session1","reasoning_effort":"medium","store":false,"tool_choice":{"function":{"name":"count"},"type":"function"},"tools":[{"function":{"name":"count","parameters":{"type":"object"},"strict":false},"type":"function"}]}
        "incomplete" {"reason":"max_output_tokens"} [{"content":[{"annotations":[],"text":"Thr","type":"output_text"}],"id":"msg_completion3","role":"assistant","status":"incomplete","type":"message"}] null
        Some(Forwarded { model: ModelName("m-small"), status: 200, request_id: Some("req_1"), usage: None, failed: Some(Failure { code: Some("max_output_tokens") }) })
        429 [("retry-after", "2"), ("content-type", "application/json")] {"error":{"code":"server_error","message":"the model provider refused the Chat request","param":null,"type":"upstream_error"}}
        400 [("content-type", "application/json")] {"error":{"code":"context_length_exceeded","message":"the model request exceeds the context window","param":null,"type":"upstream_error"}}
        400 [("content-type", "application/json")] {"error":{"code":null,"message":"`previous_response_id` refers to state kept on the provider's side, which the route refuses: send the whole context instead","param":null,"type":"invalid_request_error"}}
        400 [("content-type", "application/json")] {"error":{"code":null,"message":"Chat translation takes function tools only","param":null,"type":"invalid_request_error"}}
        400 [("content-type", "application/json")] {"error":{"code":null,"message":"Chat translation cannot carry `input_image`: omit it or use a native Responses model","param":null,"type":"invalid_request_error"}}
        400 [("content-type", "application/json")] {"error":{"code":null,"message":"Chat translation cannot carry `include`: omit it or use a native Responses model","param":null,"type":"invalid_request_error"}}
        400 [("content-type", "application/json")] {"error":{"code":null,"message":"Chat translation cannot carry `unexpected`: omit it or use a native Responses model","param":null,"type":"invalid_request_error"}}
        replayed [{"arguments":"{}","call_id":"first","id":"fc_0_c","name":"count","status":"completed","type":"function_call"},{"content":[{"annotations":[],"logprobs":[],"text":"One more.","type":"output_text"}],"id":"msg_c","role":"assistant","status":"completed","type":"message"},{"arguments":"{}","call_id":"second","id":"fc_2_c","name":"count","status":"completed","type":"function_call"}] as [{"content":"How many?","role":"user"},{"content":[{"text":"One more.","type":"text"}],"role":"assistant","tool_calls":[{"function":{"arguments":"{}","name":"count"},"id":"first","type":"function"},{"function":{"arguments":"{}","name":"count"},"id":"second","type":"function"}]},{"content":"3","role":"tool","tool_call_id":"second"},{"content":"3","role":"tool","tool_call_id":"first"}]"#]]);
}
