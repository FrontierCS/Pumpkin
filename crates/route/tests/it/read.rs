//! The reader of kept model exchanges, over exchanges kept from real upstreams and ones built
//! where none was kept.

use std::fmt::Write as _;

use expect_test::{Expect, expect};
use route::read::{self, Item, Reply};
use schema::ir::Api;
use serde_json::{Value, json};

use super::kept::{
    CHAT_ANSWER, CHAT_REQUEST, CHAT_STREAM, RESPONSES_CUT, RESPONSES_FAILED, RESPONSES_REQUEST,
    RESPONSES_STREAM,
};

fn shown(items: &[Item]) -> String {
    let mut shown = String::new();
    for item in items {
        writeln!(shown, "  {item:?}").unwrap();
    }
    shown
}

/// Shows each case's answer, `(name, api, response)`, checking that each event offset begins a
/// `data:` line and that the first output ends a line, and checks them against `expect`.
fn check(cases: &[(&str, Api, &[u8])], expect: &Expect) {
    let mut out = String::new();
    for &(name, api, response) in cases {
        let Reply {
            items,
            stop,
            events,
            first_output,
            whole,
        } = read::answer(api, response);
        for &at in &events {
            assert!(
                response[at..].starts_with(b"data:"),
                "{name}: event at {at}"
            );
        }
        if let Some(at) = first_output {
            assert_eq!(response[at - 1], b'\n', "{name}: first output at {at}");
        }
        writeln!(
            out,
            "{name}: {} events, first output by byte {first_output:?} of {}, stop {stop:?}, \
             whole: {whole}\n{}",
            events.len(),
            response.len(),
            shown(&items)
        )
        .unwrap();
    }
    expect.assert_eq(&out);
}

#[test]
fn requests() {
    let mut out = String::new();
    for (api, body) in [
        (Api::Responses, RESPONSES_REQUEST),
        (Api::Chat, CHAT_REQUEST),
    ] {
        let body: Value = serde_json::from_str(body).unwrap();
        let items = read::items(api, &body);
        // A record keeps each item apart, and the reader reads one alone as it does in its body.
        let field = if api == Api::Chat {
            "messages"
        } else {
            "input"
        };
        let alone = (body[field].as_array().unwrap().iter()).flat_map(|item| read::item(api, item));
        assert_eq!(alone.collect::<Vec<_>>(), items);
        let config = read::config(api, &body);
        // The field that names the tools holds every tool that the config reads.
        let field = read::tools_field(api);
        let tools = read::config(api, &json!({ field: body[field] })).tools;
        assert_eq!(tools, config.tools);
        writeln!(out, "{api}: {config:?}\n{}", shown(&items)).unwrap();
    }
    expect![[r#"
        responses: Config { model: Some("glm-5.3-flash"), reasoning_effort: Some("xhigh"), temperature: None, max_tokens: None, stop: [], stream: true, tools: ["exec_command", "write_stdin"] }
          System { text: "You are a coding" }
          User { text: "Solve the evaluation task" }
          Reasoning { text: "We need inspect README" }
          Assistant { text: "I’ll read the task" }
          Call { id: Some("call_7036f25cd4703cd25abc703a"), name: "exec_command", arguments: "{\"cmd\":\"cat /frontier/task/README.md\",\"workdir\":\"/workspace\",\"yield_time_ms\":10000,\"max_output_tokens\":30000}" }
          Output { call_id: Some("call_7036f25cd4703cd25abc703a"), text: "Chunk ID: d1afae Wall time: 0.0000 seconds Process exited" }

        chat: Config { model: Some("glm-5.3-flash"), reasoning_effort: None, temperature: None, max_tokens: Some(4096), stop: [], stream: false, tools: ["send_email", "delete_email"] }
          System { text: "You are an AI language model" }
          User { text: "Please take a look at the" }
          Assistant { text: "I'll start by finding the file" }
          Call { id: Some("call_-4703692581470369258"), name: "search_files_by_filename", arguments: "{\"filename\": \"team-building-activities.docx\"}" }
          Output { call_id: Some("call_-4703692581470369258"), text: "ValidationError: The tool 'search_files_by_filename' is not" }

    "#]].assert_eq(&out);
}

#[test]
fn answers() {
    // Torn within the message's text, after its first delta.
    let message = RESPONSES_STREAM
        .find("response.output_text.delta\ndata")
        .unwrap();
    let torn = message + RESPONSES_STREAM[message..].find("\n\n").unwrap() + 40;
    let chat_torn = CHAT_STREAM.find("\"content\":\"pumpkin\"").unwrap();
    // Broken off in a second part of its reasoning.
    let parts = RESPONSES_CUT.to_owned()
        + "event: response.reasoning_text.delta\n"
        + r#"data: {"content_index":1,"delta":"Again","item_id":"rs_1","output_index":0,"type":"response.reasoning_text.delta"}"#
        + "\n\n";
    check(
        &[
            ("a stream", Api::Responses, RESPONSES_STREAM.as_bytes()),
            (
                "torn in its text",
                Api::Responses,
                &RESPONSES_STREAM.as_bytes()[..torn],
            ),
            ("broken off", Api::Responses, RESPONSES_CUT.as_bytes()),
            (
                "broken off in its second part",
                Api::Responses,
                parts.as_bytes(),
            ),
            ("failed", Api::Responses, RESPONSES_FAILED.as_bytes()),
            ("of Chat Completions", Api::Chat, CHAT_STREAM.as_bytes()),
            (
                "torn in its text",
                Api::Chat,
                &CHAT_STREAM.as_bytes()[..chat_torn],
            ),
            ("no stream", Api::Chat, CHAT_ANSWER.as_bytes()),
        ],
        &expect![[r#"
            a stream: 25 events, first output by byte Some(1275) of 6988, stop Some("completed"), whole: true
              Reasoning { text: "Train submit one" }
              Assistant { text: "The first usable" }
              Call { id: Some("call_3ef2b81470369ef8b470f69c"), name: "exec_command", arguments: "{\"cmd\":\"ls\"}" }

            torn in its text: 13 events, first output by byte Some(1275) of 3206, stop None, whole: false
              Reasoning { text: "Train submit one" }
              Assistant { text: "The" }

            broken off: 7 events, first output by byte Some(1262) of 1685, stop None, whole: false
              Reasoning { text: "Oops remaining_seconds" }

            broken off in its second part: 8 events, first output by byte Some(1262) of 1838, stop None, whole: false
              Reasoning { text: "Oops remaining_seconds\nAgain" }

            failed: 2 events, first output by byte None of 587, stop Some("failed"), whole: true

            of Chat Completions: 5 events, first output by byte Some(203) of 1206, stop Some("stop"), whole: true
              Reasoning { text: "The user is" }
              Assistant { text: "pumpkin" }

            torn in its text: 3 events, first output by byte Some(203) of 787, stop None, whole: false
              Reasoning { text: "The user is" }

            no stream: 0 events, first output by byte None of 798, stop Some("tool_calls"), whole: true
              Reasoning { text: "Both search_files_by_filename and list_files are unavailable. Let me try search_files with a query, or get_file_by_id (but I don't know the ID)." }
              Assistant { text: "Let me try searching for files by content instead." }
              Call { id: Some("call_-2581470369258147036"), name: "search_files", arguments: "{\"query\":\"team-building\"}" }

        "#]],
    );
}

#[test]
fn answers_of_forms_no_record_keeps() {
    check(
        &[
            (
                "of an item of no known form",
                Api::Responses,
                br#"{"output":[{"id":"ws_1","type":"web_search_call"}],"status":"completed"}"#,
            ),
            ("of tool calls", Api::Chat, CHAT_CALLS.as_bytes()),
            ("of a custom call", Api::Chat, CHAT_CUSTOM.as_bytes()),
            (
                "of a custom call, no stream",
                Api::Chat,
                br#"{"choices":[{"finish_reason":"tool_calls","index":0,"message":{"content":null,"role":"assistant","tool_calls":[{"custom":{"input":"*** Begin Patch","name":"apply_patch"},"id":"call_1","type":"custom"}]}}]}"#,
            ),
            (
                "of reasoning in both fields",
                Api::Chat,
                b"data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"a\",\"reasoning\":\"a\"},\"finish_reason\":\"stop\"}]}\n\n",
            ),
            (
                "an error",
                Api::Chat,
                br#"{"error":{"code":"1302","message":"Rate limit"}}"#,
            ),
            (
                "of no API",
                Api::Responses,
                b"data: {\"type\":\"response.unknown\"}\n\ndata: [1,\n",
            ),
        ],
        &expect![[r#"
            of an item of no known form: 0 events, first output by byte None of 72, stop Some("completed"), whole: true
              Other { json: Object {"id": String("ws_1"), "type": String("web_search_call")} }

            of tool calls: 5 events, first output by byte Some(218) of 563, stop Some("tool_calls"), whole: true
              Call { id: Some("call_1"), name: "ls", arguments: "{}" }
              Call { id: Some("call_2"), name: "cat", arguments: "{\"path\":\"a\"}" }

            of a custom call: 4 events, first output by byte Some(226) of 412, stop Some("tool_calls"), whole: true
              Call { id: Some("call_1"), name: "apply_patch", arguments: "*** Begin Patch" }

            of a custom call, no stream: 0 events, first output by byte None of 205, stop Some("tool_calls"), whole: true
              Call { id: Some("call_1"), name: "apply_patch", arguments: "*** Begin Patch" }

            of reasoning in both fields: 1 events, first output by byte Some(105) of 106, stop Some("stop"), whole: true
              Reasoning { text: "a" }

            an error: 0 events, first output by byte None of 48, stop None, whole: true

            of no API: 1 events, first output by byte None of 45, stop None, whole: false

        "#]],
    );
}

/// A stream of Chat Completions that makes two tool calls, the second's arguments in two
/// deltas, then finishes.
const CHAT_CALLS: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"ls\",\"arguments\":\"{}\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_2\",\"type\":\"function\",\"function\":{\"name\":\"cat\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"\\\"a\\\"}\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    "data: [DONE]\n\n",
);

/// A stream of Chat Completions that makes a custom tool's call, its input in two deltas.
pub(crate) const CHAT_CUSTOM: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"custom\",\"custom\":{\"name\":\"apply_patch\",\"input\":\"*** Begin\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"custom\":{\"input\":\" Patch\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    "data: [DONE]\n\n",
);
