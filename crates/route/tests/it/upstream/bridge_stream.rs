//! Responses events observed by a caller of the Chat bridge, including both terminal usage
//! and what a caller hears when the provider ends early.

use expect_test::{Expect, expect};
use serde_json::{Value, json};

use super::{Answer, bridge_exchange};

macro_rules! chunk {
    ($fields:literal) => {
        concat!(
            "data: {\"object\":\"chat.completion.chunk\",\"id\":\"chat_1\",\"created\":123,\"model\":\"vendor/small\",",
            $fields, "}\r\n\r\n"
        )
    };
}

const CALLS: &str = concat!(
    chunk!(
        r#""choices":[{"index":0,"delta":{"role":"assistant","content":"Hé"},"finish_reason":null}]"#
    ),
    chunk!(
        r#""choices":[{"index":0,"delta":{"content":"llo","tool_calls":[{"index":0,"id":"call_","type":"function","function":{"name":"cou","arguments":""}},{"index":1,"id":"call_b","type":"function","function":{"name":"count","arguments":"["}}]}}]"#
    ),
    chunk!(
        r#""choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"nt","arguments":"{"}},{"index":1,"function":{"arguments":"]"}}]}}]"#
    ),
    chunk!(
        r#""choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"}"}}]},"finish_reason":"tool_calls"}]"#
    ),
    chunk!(
        r#""choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5,"prompt_tokens_details":{"cached_tokens":0}}"#
    ),
    "data: [DONE]\r\n\r\n"
);
const LIMITED: &str = concat!(
    chunk!(r#""choices":[{"index":0,"delta":{"refusal":"No"},"finish_reason":"length"}]"#),
    "data: [DONE]\n\n"
);
const STOP: &str = chunk!(r#""choices":[{"index":0,"delta":{},"finish_reason":"stop"}]"#);
const BAD_THEN_USAGE: &str = concat!(
    "data: {broken\n\n",
    chunk!(r#""choices":[],"usage":{"prompt_tokens":20,"completion_tokens":5,"total_tokens":25}"#)
);
const AUDIO: &str =
    chunk!(r#""choices":[{"index":0,"delta":{"audio":{"data":"abc"}},"finish_reason":"stop"}]"#);
const FIRST_ERROR: &str = "event: error\rdata: {\"code\":\"context_length_exceeded\",\rdata: \"message\":\"private upstream message\"}\r\r";
const ZERO: &str = concat!(
    chunk!(r#""choices":[{"index":0,"delta":{},"finish_reason":"stop"}]"#),
    "id: usage\r\nretry: 1000\r\n: keepalive\r\nx-extension: 1\r\n",
    "data: {\"object\":\"chat.completion.chunk\",\"id\":\"chat_1\",\r\n",
    "data: \"model\":\"vendor/small\",\"created\":123,\"choices\":[],\r\n",
    "data: \"usage\":{\"prompt_tokens\":0,\"completion_tokens\":0,\"total_tokens\":0}}\r\n\r\n",
    "data: [DONE]\n\n"
);

fn event_line(event: &Value) -> String {
    let mut line = event["type"]
        .as_str()
        .unwrap()
        .strip_prefix("response.")
        .unwrap()
        .to_owned();
    for value in [
        &event["output_index"],
        &event["content_index"],
        &event["item"]["id"],
        &event["item_id"],
        &event["delta"],
    ] {
        if !value.is_null() {
            line.push(' ');
            line.push_str(&value.to_string());
        }
    }
    line
}

fn check_local_failure(events: &[Value]) {
    let [created, progress, failed] = events else {
        panic!("expected the failure lifecycle")
    };
    assert_eq!(created["type"], "response.created");
    assert_eq!(progress["type"], "response.in_progress");
    assert_eq!(failed["type"], "response.failed");
    let id = created["response"]["id"].as_str().unwrap();
    let random = id.strip_prefix("resp_pumpkin_").unwrap();
    assert_eq!(random.len(), 32);
    assert!(random.bytes().all(|b| b.is_ascii_hexdigit()));
    let at = created["response"]["created_at"].as_u64().unwrap();
    assert!(at > 0);
    for event in events {
        assert_eq!(event["response"]["id"], id);
        assert_eq!(event["response"]["created_at"], at);
        assert_eq!(event["response"]["output"], json!([]));
        assert_eq!(event["response"]["usage"], Value::Null);
    }
}

fn check(cases: &[(&str, [&'static str; 2])], expect: &Expect) {
    let mut shown = Vec::new();
    for (case, parts) in cases {
        let (heard, told, request, _) = bridge_exchange(
            json!({"model":"m-small","store":false,"stream":true,"input":"Words?"}),
            Some(Answer::Streams(*parts)),
        );
        let request: Value = serde_json::from_slice(&request).unwrap();
        assert_eq!(request["stream_options"]["include_usage"], true);
        let (headers, body) = heard.split_once("] ").unwrap();
        assert!(
            headers.starts_with("200 ") && headers.contains("text/event-stream"),
            "{heard}"
        );
        let mut events = Vec::new();
        for (sequence, frame) in body.trim_end().split("\n\n").enumerate() {
            let (kind, data) = frame.split_once("\ndata: ").unwrap();
            let event: Value = serde_json::from_str(data).unwrap();
            assert_eq!(event["sequence_number"], sequence);
            assert_eq!(kind.strip_prefix("event: ").unwrap(), event["type"]);
            if event["response"].is_object() {
                if parts[0] != FIRST_ERROR {
                    assert_eq!(event["response"]["id"], "chat_1");
                }
                assert_eq!(event["response"]["model"], "m-small");
            }
            events.push(event);
        }
        let (terminal, prior) = events.split_last().unwrap();
        if parts[0] == FIRST_ERROR {
            check_local_failure(&events);
        }
        let body = &terminal["response"];
        let events = if body["status"] == "failed" {
            assert!(
                prior
                    .iter()
                    .all(|e| e["type"].as_str().unwrap().rsplit('.').next() != Some("done"))
            );
            format!("failure after {} events", prior.len())
        } else {
            prior.iter().map(event_line).collect::<Vec<_>>().join("\n")
        };
        shown.push(format!(
            "{case}\n{}\n{} {} {} {} {}\n{told:?}",
            events,
            terminal["type"],
            body["output"],
            body["usage"],
            body["incomplete_details"],
            body["error"]
        ));
    }
    expect.assert_eq(&shown.join("\n"));
}

#[test]
fn interleaved_chat_calls_keep_their_responses_positions() {
    let (first, second) = CALLS.split_at(CALLS.find("llo").unwrap() + 1);
    check(
        &[("text and two calls", [first, second])],
        &expect![[r#"
            text and two calls
            created
            in_progress
            output_item.added 0 "msg_chat_1"
            content_part.added 0 0 "msg_chat_1"
            output_text.delta 0 0 "msg_chat_1" "Hé"
            output_text.delta 0 0 "msg_chat_1" "llo"
            output_item.added 1 "fc_1_chat_1"
            function_call_arguments.delta 1 "fc_1_chat_1" "{"
            output_item.added 2 "fc_2_chat_1"
            function_call_arguments.delta 2 "fc_2_chat_1" "["
            function_call_arguments.delta 2 "fc_2_chat_1" "]"
            function_call_arguments.delta 1 "fc_1_chat_1" "}"
            output_text.done 0 0 "msg_chat_1"
            content_part.done 0 0 "msg_chat_1"
            output_item.done 0 "msg_chat_1"
            function_call_arguments.done 1 "fc_1_chat_1"
            output_item.done 1 "fc_1_chat_1"
            function_call_arguments.done 2 "fc_2_chat_1"
            output_item.done 2 "fc_2_chat_1"
            "response.completed" [{"content":[{"annotations":[],"logprobs":[],"text":"Héllo","type":"output_text"}],"id":"msg_chat_1","role":"assistant","status":"completed","type":"message"},{"arguments":"{}","call_id":"call_a","id":"fc_1_chat_1","name":"count","status":"completed","type":"function_call"},{"arguments":"[]","call_id":"call_b","id":"fc_2_chat_1","name":"count","status":"completed","type":"function_call"}] {"input_tokens":3,"input_tokens_details":{"cache_write_tokens":0,"cached_tokens":0},"output_tokens":2,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":5} null null
            Some(Forwarded { model: ModelName("m-small"), status: 200, request_id: Some("req_1"), usage: Some(Usage { input_tokens: 3, output_tokens: 2, cached_tokens: Some(0), cache_write_tokens: None, reasoning_tokens: None }), failed: None })"#]],
    );
}

#[test]
fn chat_stream_endings_reach_the_caller_and_record() {
    let start = CALLS.split_inclusive("\r\n\r\n").next().unwrap();
    check(
        &[
            ("refusal and token limit", [start, LIMITED]),
            ("reported zero in multiline SSE", [start, ZERO]),
            (
                "error before the first choice",
                [FIRST_ERROR, ": close\n\n"],
            ),
            (
                "upstream error",
                [
                    start,
                    "data: {\"error\":{\"message\":\"private upstream message\",\"code\":\"rate_limit_exceeded\"}}\n\n",
                ],
            ),
            ("missing done after finish reason", [start, STOP]),
            ("malformed data then billed usage", [start, BAD_THEN_USAGE]),
            ("unsupported delta", [start, AUDIO]),
        ],
        &expect![[r#"
            refusal and token limit
            created
            in_progress
            output_item.added 0 "msg_chat_1"
            content_part.added 0 0 "msg_chat_1"
            output_text.delta 0 0 "msg_chat_1" "Hé"
            content_part.added 0 1 "msg_chat_1"
            refusal.delta 0 1 "msg_chat_1" "No"
            output_text.done 0 0 "msg_chat_1"
            content_part.done 0 0 "msg_chat_1"
            refusal.done 0 1 "msg_chat_1"
            content_part.done 0 1 "msg_chat_1"
            output_item.done 0 "msg_chat_1"
            "response.incomplete" [{"content":[{"annotations":[],"logprobs":[],"text":"Hé","type":"output_text"},{"refusal":"No","type":"refusal"}],"id":"msg_chat_1","role":"assistant","status":"incomplete","type":"message"}] null {"reason":"max_output_tokens"} null
            Some(Forwarded { model: ModelName("m-small"), status: 200, request_id: Some("req_1"), usage: None, failed: Some(Failure { code: Some("max_output_tokens") }) })
            reported zero in multiline SSE
            created
            in_progress
            output_item.added 0 "msg_chat_1"
            content_part.added 0 0 "msg_chat_1"
            output_text.delta 0 0 "msg_chat_1" "Hé"
            output_text.done 0 0 "msg_chat_1"
            content_part.done 0 0 "msg_chat_1"
            output_item.done 0 "msg_chat_1"
            "response.completed" [{"content":[{"annotations":[],"logprobs":[],"text":"Hé","type":"output_text"}],"id":"msg_chat_1","role":"assistant","status":"completed","type":"message"}] {"input_tokens":0,"input_tokens_details":{"cache_write_tokens":0,"cached_tokens":0},"output_tokens":0,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":0} null null
            Some(Forwarded { model: ModelName("m-small"), status: 200, request_id: Some("req_1"), usage: Some(Usage { input_tokens: 0, output_tokens: 0, cached_tokens: None, cache_write_tokens: None, reasoning_tokens: None }), failed: None })
            error before the first choice
            failure after 2 events
            "response.failed" [] null null {"code":"context_length_exceeded","message":"the model request exceeds the context window"}
            Some(Forwarded { model: ModelName("m-small"), status: 200, request_id: Some("req_1"), usage: None, failed: Some(Failure { code: Some("context_length_exceeded") }) })
            upstream error
            failure after 5 events
            "response.failed" [{"content":[{"annotations":[],"logprobs":[],"text":"Hé","type":"output_text"}],"id":"msg_chat_1","role":"assistant","status":"incomplete","type":"message"}] null null {"code":"rate_limit_exceeded","message":"the model provider rate-limited the request"}
            Some(Forwarded { model: ModelName("m-small"), status: 200, request_id: Some("req_1"), usage: None, failed: Some(Failure { code: Some("rate_limit_exceeded") }) })
            missing done after finish reason
            failure after 5 events
            "response.failed" [{"content":[{"annotations":[],"logprobs":[],"text":"Hé","type":"output_text"}],"id":"msg_chat_1","role":"assistant","status":"incomplete","type":"message"}] null null {"code":"server_error","message":"the model provider's Chat stream broke off"}
            Some(Cut { model: ModelName("m-small"), status: 200, request_id: Some("req_1"), usage: None, failed: Some(Failure { code: Some("incomplete_chat_stream") }) })
            malformed data then billed usage
            failure after 5 events
            "response.failed" [{"content":[{"annotations":[],"logprobs":[],"text":"Hé","type":"output_text"}],"id":"msg_chat_1","role":"assistant","status":"incomplete","type":"message"}] null null {"code":"server_error","message":"invalid JSON in Chat stream"}
            Some(Cut { model: ModelName("m-small"), status: 200, request_id: Some("req_1"), usage: Some(Usage { input_tokens: 20, output_tokens: 5, cached_tokens: None, cache_write_tokens: None, reasoning_tokens: None }), failed: Some(Failure { code: Some("invalid_chat_stream") }) })
            unsupported delta
            failure after 5 events
            "response.failed" [{"content":[{"annotations":[],"logprobs":[],"text":"Hé","type":"output_text"}],"id":"msg_chat_1","role":"assistant","status":"incomplete","type":"message"}] null null {"code":"server_error","message":"unsupported Chat output in delta"}
            Some(Cut { model: ModelName("m-small"), status: 200, request_id: Some("req_1"), usage: None, failed: Some(Failure { code: Some("invalid_chat_stream") }) })"#]],
    );
}

#[test]
fn archived_chat_skips_an_oversized_record_to_its_end() {
    let mut archive = Vec::new();
    for _ in 0..2 {
        archive.extend_from_slice(b"data: ");
        archive.resize(archive.len() + (8 << 20), b'x');
        archive.extend_from_slice(b"\r\n");
    }
    // A data line after the bound, still in the oversized record.
    archive.extend_from_slice(b"data: {}\r\n\r\nid: next\r\n");
    let start = archive.len();
    archive.extend_from_slice(b"data: {\"choices\":[\r\n");
    archive.extend_from_slice(
        b"data: {\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\r\n",
    );
    archive.extend_from_slice(b": note\r\n\r");
    let end = archive.len();
    archive.extend_from_slice(b"\ndata: [DONE]\r\n\r\n");
    let reply = route::read::answer(schema::ir::Api::Chat, &archive);
    assert_eq!(reply.events, [start]);
    assert_eq!(reply.first_output, Some(end));
    assert_eq!(
        reply.items,
        [route::read::Item::Assistant {
            text: "ok".to_owned()
        }]
    );
}
