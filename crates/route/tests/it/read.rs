//! The reader of kept model exchanges, over exchanges kept from real upstreams and ones built
//! where none was kept.

use std::fmt::Write as _;

use expect_test::{Expect, expect};
use route::read::{self, Item, Reply};
use schema::ir::Api;
use serde_json::{Value, json};

use super::kept::{
    CHAT_ANSWER, CHAT_REQUEST, CHAT_STREAM, GENERATE_CONTENT_ANSWER, GENERATE_CONTENT_REQUEST,
    GENERATE_CONTENT_STREAM, MESSAGES_ANSWER, MESSAGES_CUT, MESSAGES_FAILED, MESSAGES_REQUEST,
    MESSAGES_STREAM, RESPONSES_CUT, RESPONSES_FAILED, RESPONSES_REQUEST, RESPONSES_STREAM,
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
        (Api::Messages, MESSAGES_REQUEST),
        (Api::GenerateContent, GENERATE_CONTENT_REQUEST),
    ] {
        let body: Value = serde_json::from_str(body).unwrap();
        let items = read::items(api, &body);
        // A record keeps each item apart, and the reader reads one alone as it does in its body,
        // after the entries of the fields it reads first, such as the Messages API's `system`.
        let field = match api {
            Api::Responses => "input",
            Api::Chat | Api::Messages => "messages",
            Api::GenerateContent => "contents",
        };
        let alone = (body[field].as_array().unwrap().iter()).flat_map(|item| read::item(api, item));
        assert!(items.ends_with(&alone.collect::<Vec<_>>()), "{api}");
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
          Call { id: Some("call_7036f25cd4703cd25abc703a"), name: "exec_command", arguments: "{\"cmd\":\"cat /task/README.md\",\"workdir\":\"/workspace\",\"yield_time_ms\":10000,\"max_output_tokens\":30000}" }
          Output { call_id: Some("call_7036f25cd4703cd25abc703a"), text: "Chunk ID: d1afae Wall time: 0.0000 seconds Process exited", error: false }

        chat: Config { model: Some("glm-5.3-flash"), reasoning_effort: None, temperature: None, max_tokens: Some(4096), stop: [], stream: false, tools: ["send_email", "delete_email"] }
          System { text: "You are an AI language model" }
          User { text: "Please take a look at the" }
          Assistant { text: "I'll start by finding the file" }
          Call { id: Some("call_-4703692581470369258"), name: "search_files_by_filename", arguments: "{\"filename\": \"team-building-activities.docx\"}" }
          Output { call_id: Some("call_-4703692581470369258"), text: "ValidationError: The tool 'search_files_by_filename' is not", error: false }

        messages: Config { model: Some("m-large"), reasoning_effort: Some("high"), temperature: Some(1.0), max_tokens: Some(32000), stop: ["</done>"], stream: true, tools: ["Bash", "Read"] }
          System { text: "You are Claude Code\nWork in /workspace" }
          User { text: "Solve the task" }
          Other { json: Object {"source": Object {"data": String("AAAA"), "media_type": String("image/png"), "type": String("base64")}, "type": String("image")} }
          Reasoning { text: "I should list the files" }
          Assistant { text: "Listing the files." }
          Call { id: Some("toolu_01A"), name: "Bash", arguments: "{\"command\":\"ls\"}" }
          Output { call_id: Some("toolu_01A"), text: "ls: cannot open", error: true }
          User { text: "Go on" }
          System { text: "Finish soon" }
          System { text: "Primary working directory: /workspace" }
          Assistant { text: "Done." }

        generate_content: Config { model: None, reasoning_effort: Some("LOW"), temperature: Some(1.0), max_tokens: Some(2000), stop: [], stream: false, tools: ["run_command"] }
          System { text: "You are a coding agent. Think briefly, say in a few words what you will do, then use the tool." }
          User { text: "What is this machine's hostname?" }
          Assistant { text: "I will run `hostname` to check the machine's hostname." }
          Call { id: Some("call_1470369258147036"), name: "run_command", arguments: "{\"command\":\"hostname\"}" }
          Output { call_id: Some("call_1470369258147036"), text: "{\"output\":\"box-17\\n\"}", error: false }

    "#]].assert_eq(&out);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn answers() {
    // Torn within the message's text, after its first delta.
    let message = RESPONSES_STREAM
        .find("response.output_text.delta\ndata")
        .unwrap();
    let torn = message + RESPONSES_STREAM[message..].find("\n\n").unwrap() + 40;
    let chat_torn = CHAT_STREAM.find("\"content\":\"pumpkin\"").unwrap();
    // Within the line of its text's second delta.
    let messages_torn = MESSAGES_STREAM.find(" them.").unwrap();
    let generate_content_torn = GENERATE_CONTENT_STREAM.find("\"functionCall\"").unwrap();
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
            ("of Messages", Api::Messages, MESSAGES_STREAM.as_bytes()),
            (
                "torn in its text",
                Api::Messages,
                &MESSAGES_STREAM.as_bytes()[..messages_torn],
            ),
            (
                "broken off in a call",
                Api::Messages,
                MESSAGES_CUT.as_bytes(),
            ),
            ("failed", Api::Messages, MESSAGES_FAILED.as_bytes()),
            ("no stream", Api::Messages, MESSAGES_ANSWER.as_bytes()),
            (
                "of the Gemini API",
                Api::GenerateContent,
                GENERATE_CONTENT_STREAM.as_bytes(),
            ),
            (
                "torn in its call",
                Api::GenerateContent,
                &GENERATE_CONTENT_STREAM.as_bytes()[..generate_content_torn],
            ),
            (
                "no stream",
                Api::GenerateContent,
                GENERATE_CONTENT_ANSWER.as_bytes(),
            ),
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

            of Messages: 18 events, first output by byte Some(604) of 2207, stop Some("tool_use"), whole: true
              Reasoning { text: "The tests fail" }
              Assistant { text: "Running them." }
              Call { id: Some("toolu_01B"), name: "Bash", arguments: "{\"command\": \"cargo test\"}" }

            torn in its text: 9 events, first output by byte Some(604) of 1292, stop None, whole: false
              Reasoning { text: "The tests fail" }
              Assistant { text: "Running" }

            broken off in a call: 3 events, first output by byte Some(385) of 525, stop None, whole: false
              Call { id: Some("toolu_01C"), name: "Read", arguments: "{\"path\":" }

            failed: 2 events, first output by byte None of 335, stop None, whole: false

            no stream: 0 events, first output by byte None of 357, stop Some("tool_use"), whole: true
              Assistant { text: "I'll read it." }
              Call { id: Some("toolu_01D"), name: "Read", arguments: "{\"path\":\"README.md\"}" }

            of the Gemini API: 3 events, first output by byte Some(440) of 1353, stop Some("STOP"), whole: true
              Assistant { text: "I will run `hostname` to check the machine's hostname." }
              Call { id: Some("call_1470369258147036"), name: "run_command", arguments: "{\"command\":\"hostname\"}" }

            torn in its call: 1 events, first output by byte Some(440) of 484, stop None, whole: false
              Assistant { text: "I will run `hostname` to check the machine's hostname." }

            no stream: 0 events, first output by byte None of 477, stop Some("STOP"), whole: true
              Assistant { text: "This machine's hostname is `box-17`." }

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
                "of hidden thinking, no stream",
                Api::Messages,
                br#"{"content":[{"data":"EmwKAhgB","type":"redacted_thinking"}],"stop_reason":"end_turn","type":"message"}"#,
            ),
            (
                "of a call with no input's deltas",
                Api::Messages,
                b"data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Stop\",\"input\":{}}}\n\ndata: {\"type\":\"message_stop\"}\n\n",
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

            of hidden thinking, no stream: 0 events, first output by byte None of 102, stop Some("end_turn"), whole: true
              Reasoning { text: "" }

            of a call with no input's deltas: 2 events, first output by byte Some(123) of 155, stop None, whole: true
              Call { id: Some("toolu_1"), name: "Stop", arguments: "{}" }

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

// A body is the provider's, so the reader takes any: each prefix of a stream, and forms of the
// wrong type or index, give what they hold and never panic.
#[test]
fn messages_of_any_form() {
    // Only a prefix that holds the line of the last event, `message_stop`, is a whole answer.
    let last_line_at = MESSAGES_STREAM.len() - "\n".len();
    for at in 0..=MESSAGES_STREAM.len() {
        let reply = read::answer(Api::Messages, &MESSAGES_STREAM.as_bytes()[..at]);
        assert_eq!(reply.whole, at >= last_line_at, "a stream cut at byte {at}");
    }
    for at in 0..=MESSAGES_REQUEST.len() {
        if let Ok(body) = serde_json::from_str::<Value>(&MESSAGES_REQUEST[..at]) {
            read::items(Api::Messages, &body);
        }
    }
    // The last is of the right form: a thinking budget, and a system prompt of one text.
    let wrong = [
        r#"{"system":7,"messages":[{"role":"user","content":7},{"role":3,"content":[7,"a",{"type":7}]}]}"#,
        r#"{"messages":{"role":"user"},"tools":"Bash","thinking":"high","stop_sequences":"x","output_config":7}"#,
        r#"[1,2]"#,
        r#"{"thinking":{"type":"enabled","budget_tokens":2048},"system":"Be brief"}"#,
    ];
    let mut out = String::new();
    for body in wrong {
        let body: Value = serde_json::from_str(body).unwrap();
        let config = read::config(Api::Messages, &body);
        writeln!(
            out,
            "{config:?}\n{}",
            shown(&read::items(Api::Messages, &body))
        )
        .unwrap();
    }
    expect![[r#"
        Config { model: None, reasoning_effort: None, temperature: None, max_tokens: None, stop: [], stream: false, tools: [] }
          Other { json: Object {"content": Number(7), "role": String("user")} }
          Other { json: Number(7) }
          Other { json: String("a") }
          Other { json: Object {"type": Number(7)} }

        Config { model: None, reasoning_effort: None, temperature: None, max_tokens: None, stop: [], stream: false, tools: [] }

        Config { model: None, reasoning_effort: None, temperature: None, max_tokens: None, stop: [], stream: false, tools: [] }

        Config { model: None, reasoning_effort: Some("2048"), temperature: None, max_tokens: None, stop: [], stream: false, tools: [] }
          System { text: "Be brief" }

    "#]].assert_eq(&out);
}

#[test]
fn messages_resent_unmarked() {
    let mark = json!({ "type": "ephemeral" });
    // As Claude Code sends a conversation: it marks the last block of each request, as a text
    // block where it is a text, and sends the message it marked before as it was.
    let first = [json!({ "role": "user",
        "content": [{ "type": "text", "text": "Solve the task", "cache_control": mark }] })];
    let next = [
        json!({ "role": "user", "content": "Solve the task" }),
        json!({ "role": "assistant", "content": [{ "type": "text", "text": "Listing." },
            { "type": "tool_use", "id": "toolu_01A", "name": "Bash", "input": { "command": "ls" } }] }),
        json!({ "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "toolu_01A",
            "content": "a.txt", "cache_control": mark }] }),
    ];
    let unmarked = |items: &[Value]| -> Vec<Value> {
        let unmarked = items.iter().map(|item| read::unmarked(Api::Messages, item));
        unmarked.collect()
    };
    assert!(unmarked(&next).starts_with(&unmarked(&first)));
    let others = [
        json!({ "role": "assistant", "content": [{ "type": "text", "text": "Done.", "citations": [] }] }),
        json!({ "role": "user", "content": [{ "type": "text", "text": "Go on" },
            { "type": "text", "text": "Finish soon", "cache_control": mark }] }),
        json!({ "role": "user", "content": [{ "type": "image", "cache_control": mark,
            "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" } }] }),
    ];
    let mut out = String::new();
    for item in first.iter().chain(&next).chain(&others) {
        writeln!(out, "{}", read::unmarked(Api::Messages, item)).unwrap();
    }
    // Another API's item is as it was sent, whatever it holds.
    let responses = json!({ "type": "message", "role": "user",
        "content": [{ "type": "input_text", "text": "Solve the task", "cache_control": mark }] });
    writeln!(out, "{}", read::unmarked(Api::Responses, &responses)).unwrap();
    expect![[r#"
        {"content":"Solve the task","role":"user"}
        {"content":"Solve the task","role":"user"}
        {"content":[{"text":"Listing.","type":"text"},{"id":"toolu_01A","input":{"command":"ls"},"name":"Bash","type":"tool_use"}],"role":"assistant"}
        {"content":[{"content":"a.txt","tool_use_id":"toolu_01A","type":"tool_result"}],"role":"user"}
        {"content":[{"citations":[],"text":"Done.","type":"text"}],"role":"assistant"}
        {"content":[{"text":"Go on","type":"text"},{"text":"Finish soon","type":"text"}],"role":"user"}
        {"content":[{"source":{"data":"AAAA","media_type":"image/png","type":"base64"},"type":"image"}],"role":"user"}
        {"content":[{"cache_control":{"type":"ephemeral"},"text":"Solve the task","type":"input_text"}],"role":"user","type":"message"}
    "#]].assert_eq(&out);
}

#[test]
fn messages_answers_of_any_form() {
    check(
        &[
            (
                "of wrong types and a huge index",
                Api::Messages,
                concat!(
                    "data: {\"type\":\"content_block_start\",\"index\":18446744073709551615,\"content_block\":\"text\"}\n\n",
                    "data: {\"type\":\"content_block_delta\",\"index\":-1,\"delta\":{\"type\":\"text_delta\",\"text\":7}}\n\n",
                    "data: {\"type\":\"content_block_delta\",\"index\":\"1\",\"delta\":[]}\n\n",
                    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":7}}\n\n",
                    "data: [\"message_stop\"]\n\n",
                )
                .as_bytes(),
            ),
            (
                "of a body of no message",
                Api::Messages,
                br#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            ),
        ],
        &expect![[r#"
            of wrong types and a huge index: 5 events, first output by byte None of 321, stop None, whole: false
              Other { json: Null }
              Other { json: String("text") }

            of a body of no message: 0 events, first output by byte None of 75, stop None, whole: true

        "#]],
    );
}
