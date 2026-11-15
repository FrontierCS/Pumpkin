//! The Messages API's forms: its messages' content blocks, and its stream's events.
//!
//! A message's content is a text or a list of blocks, each an entry: a text by the message's role,
//! thinking, a tool call, or a tool's result. A stream's answer is its content blocks, in the order
//! of their index, each the block that its `content_block_start` gives with the text of its
//! deltas, as the API's documentation joins them:
//! <https://platform.claude.com/docs/en/build-with-claude/streaming#event-types>.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::{Config, Dialect, Item, Streamed, config_of, joined, string, text};

pub(super) const DIALECT: Dialect = Dialect {
    output,
    item,
    items,
    body,
    stream,
    config,
};

/// A Messages event that is output: a content block's delta that is not empty, of text, of
/// thinking or of a tool call's input, or the start of a tool call's block, which names it before
/// its input comes.
// A delta of a thinking block's signature, which a block of thinking that is not shown holds
// alone, passes no text of the model's:
// https://platform.claude.com/docs/en/build-with-claude/streaming#thinking-delta
fn output(event: &Value) -> bool {
    match event["type"].as_str() {
        Some("content_block_delta") => {
            let delta = &event["delta"];
            delta_text(delta).is_some_and(|said| !said.is_empty())
        }
        Some("content_block_start") => event["content_block"]["type"] == "tool_use",
        _ => false,
    }
}

/// The text that `delta`, a content block's, adds: of text, of thinking, or of a tool call's
/// input JSON.
fn delta_text(delta: &Value) -> Option<&str> {
    let field = match delta["type"].as_str()? {
        "text_delta" => "text",
        "thinking_delta" => "thinking",
        "input_json_delta" => "partial_json",
        _ => return None,
    };
    delta[field].as_str()
}

fn item(message: &Value) -> Vec<Item> {
    let role = message["role"].as_str();
    match &message["content"] {
        Value::String(content) => vec![said(role, content.clone(), message)],
        Value::Array(blocks) => blocks.iter().map(|one| block(role, one)).collect(),
        _ => vec![Item::Other {
            json: message.clone(),
        }],
    }
}

/// The text `content` as `role` said it; a role not known gives `json`, the message or block, as
/// it is.
fn said(role: Option<&str>, content: String, json: &Value) -> Item {
    match role {
        Some("user") => Item::User { text: content },
        Some("assistant") => Item::Assistant { text: content },
        Some("system") => Item::System { text: content },
        _ => Item::Other { json: json.clone() },
    }
}

fn block(role: Option<&str>, block: &Value) -> Item {
    let text_of = |field: &str| string(block, field).unwrap_or_default();
    match block["type"].as_str() {
        Some("text") => said(role, text_of("text"), block),
        Some("thinking") => Item::Reasoning {
            text: text_of("thinking"),
        },
        // Its thinking is encrypted.
        Some("redacted_thinking") => Item::Reasoning {
            text: String::new(),
        },
        Some("tool_use") => Item::Call {
            id: string(block, "id"),
            name: text_of("name"),
            arguments: block.get("input").map(Value::to_string).unwrap_or_default(),
        },
        Some("tool_result") => Item::Output {
            call_id: string(block, "tool_use_id"),
            text: text(&block["content"]),
            error: block["is_error"] == true,
        },
        _ => Item::Other {
            json: block.clone(),
        },
    }
}

fn items(body: &Map<String, Value>) -> Vec<Item> {
    let system = match body.get("system") {
        Some(Value::String(system)) => Some(system.clone()),
        Some(Value::Array(blocks)) => Some(joined(blocks)),
        _ => None,
    };
    let system = system.map(|text| Item::System { text });
    let messages = body.get("messages").and_then(Value::as_array);
    let messages = messages.into_iter().flatten().flat_map(item);
    system.into_iter().chain(messages).collect()
}

/// A message's content blocks, and why it stopped.
fn body(body: &Value) -> (Vec<Item>, Option<String>) {
    let blocks = body["content"].as_array().into_iter().flatten();
    let items = blocks.map(|one| block(Some("assistant"), one)).collect();
    (items, string(body, "stop_reason"))
}

#[derive(Default)]
struct Joined {
    start: Value,
    deltas: String,
}

/// The stream's content blocks so far, each with the text of its deltas, why it stopped, and
/// whether it reached its `message_stop`.
fn stream(events: &[Value]) -> Streamed {
    let mut blocks: BTreeMap<u64, Joined> = BTreeMap::new();
    let mut stop = None;
    let mut whole = false;
    for event in events {
        let index = event["index"].as_u64().unwrap_or_default();
        match event["type"].as_str() {
            Some("content_block_start") => {
                blocks.entry(index).or_default().start = event["content_block"].clone();
            }
            Some("content_block_delta") => {
                let deltas = &mut blocks.entry(index).or_default().deltas;
                *deltas += delta_text(&event["delta"]).unwrap_or_default();
            }
            Some("message_delta") => {
                stop = string(&event["delta"], "stop_reason").or(stop);
            }
            Some("message_stop") => whole = true,
            _ => {}
        }
    }
    let items = blocks.into_values().map(|Joined { start, deltas }| {
        let begun = |field: &str| string(&start, field).unwrap_or_default();
        match start["type"].as_str() {
            Some("text") => Item::Assistant {
                text: begun("text") + &deltas,
            },
            Some("thinking") => Item::Reasoning {
                text: begun("thinking") + &deltas,
            },
            // Its input comes in its deltas, and is the one its start gives only if none came.
            Some("tool_use") if !deltas.is_empty() => Item::Call {
                id: string(&start, "id"),
                name: begun("name"),
                arguments: deltas,
            },
            _ => block(Some("assistant"), &start),
        }
    });
    (items.collect(), stop, whole)
}

fn config(body: &Map<String, Value>) -> Config {
    let effort = body
        .get("output_config")
        .and_then(|output| output.get("effort"));
    let mut config = config_of(body, effort, &["max_tokens"]);
    if config.reasoning_effort.is_none() {
        let budget = body
            .get("thinking")
            .and_then(|thinking| thinking.get("budget_tokens"));
        config.reasoning_effort = budget
            .and_then(Value::as_u64)
            .map(|budget| budget.to_string());
    }
    let stops = body.get("stop_sequences").and_then(Value::as_array);
    let stops = stops.into_iter().flatten().filter_map(Value::as_str);
    config.stop = stops.map(str::to_owned).collect();
    config
}
