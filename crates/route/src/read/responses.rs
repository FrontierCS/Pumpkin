//! The Responses API's forms: its input items, its output items and its stream's events.
//!
//! An answer is its last event's `response.output`, which a stream that ends gives whole; else
//! each item that the stream finished (`response.output_item.done`), and each it added and has
//! not finished, with the text of its deltas so far.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::{Config, Dialect, Item, Streamed, config_of, joined, string, text};

pub(super) const DIALECT: Dialect = Dialect {
    output,
    item: |item| vec![entry(item)],
    items,
    body,
    stream,
    config,
};

/// The events that end a stream, each with its `response`.
const ENDS: [&str; 3] = [
    "response.completed",
    "response.incomplete",
    "response.failed",
];

/// The events that add to the text of the item at their `output_index`.
const DELTAS: [&str; 6] = [
    "response.output_text.delta",
    "response.refusal.delta",
    "response.reasoning_text.delta",
    "response.reasoning_summary_text.delta",
    "response.function_call_arguments.delta",
    "response.custom_tool_call_input.delta",
];

/// A Responses event that is output: a delta that is not empty, of text, a refusal, reasoning,
/// or a call's arguments or input, or the item added of a call, which names it before its
/// arguments come.
// A custom tool call, such as Codex's `apply_patch`, streams as its item added and its input's
// deltas, as Codex reads them:
// https://github.com/openai/codex/blob/09db085f9456c608a7960fdd4c5c55a34c6a84ea/codex-rs/codex-api/src/sse/responses.rs#L399-L408
fn output(event: &Value) -> bool {
    match event["type"].as_str() {
        Some(kind) if DELTAS.contains(&kind) => event["delta"]
            .as_str()
            .is_some_and(|delta| !delta.is_empty()),
        Some("response.output_item.added") => {
            matches!(
                event["item"]["type"].as_str(),
                Some("function_call" | "custom_tool_call")
            )
        }
        _ => false,
    }
}

fn entry(item: &Value) -> Item {
    match item["type"].as_str() {
        Some("message") | None => match item["role"].as_str() {
            Some("system" | "developer") => Item::System {
                text: text(&item["content"]),
            },
            Some("user") => Item::User {
                text: text(&item["content"]),
            },
            Some("assistant") => Item::Assistant {
                text: text(&item["content"]),
            },
            _ => Item::Other { json: item.clone() },
        },
        Some("reasoning") => {
            let parts = ["summary", "content"]
                .iter()
                .flat_map(|field| item[*field].as_array().into_iter().flatten());
            Item::Reasoning {
                text: joined(parts),
            }
        }
        Some("function_call") => Item::Call {
            id: string(item, "call_id"),
            name: string(item, "name").unwrap_or_default(),
            arguments: string(item, "arguments").unwrap_or_default(),
        },
        Some("custom_tool_call") => Item::Call {
            id: string(item, "call_id"),
            name: string(item, "name").unwrap_or_default(),
            arguments: string(item, "input").unwrap_or_default(),
        },
        Some("function_call_output" | "custom_tool_call_output") => Item::Output {
            call_id: string(item, "call_id"),
            text: text(&item["output"]),
            error: false,
        },
        Some(_) => Item::Other { json: item.clone() },
    }
}

/// The instructions, as the system's first item, then the input: a string is one user item.
fn items(body: &Map<String, Value>) -> Vec<Item> {
    let mut items = Vec::new();
    if let Some(Value::String(text)) = body.get("instructions")
        && !text.is_empty()
    {
        let text = text.clone();
        items.push(Item::System { text });
    }
    match body.get("input") {
        Some(Value::String(text)) => {
            let text = text.clone();
            items.push(Item::User { text });
        }
        Some(Value::Array(input)) => items.extend(input.iter().map(entry)),
        _ => {}
    }
    items
}

fn body(body: &Value) -> (Vec<Item>, Option<String>) {
    let items = body["output"].as_array().into_iter().flatten();
    (items.map(entry).collect(), stop(body))
}

/// Why `response` stopped: the reason it gives for being incomplete, else its status.
fn stop(response: &Value) -> Option<String> {
    let reason = response.pointer("/incomplete_details/reason");
    (reason.and_then(Value::as_str).map(str::to_owned)).or_else(|| string(response, "status"))
}

/// An item of the output, by its index: as the stream finished it, or as it added it, with the
/// text of its deltas so far and the part of the item that the last of them added to.
enum Made<'a> {
    Done(&'a Value),
    Making {
        added: &'a Value,
        text: String,
        part: Option<Part>,
    },
}

/// A delta's `summary_index` and `content_index`.
type Part = (Option<u64>, Option<u64>);

fn stream(events: &[Value]) -> Streamed {
    let mut made = BTreeMap::new();
    let mut end = None;
    for event in events {
        let index = event["output_index"].as_u64();
        match (event["type"].as_str(), index) {
            (Some("response.output_item.added"), Some(index)) => {
                let added = &event["item"];
                let (text, part) = (String::new(), None);
                made.insert(index, Made::Making { added, text, part });
            }
            (Some("response.output_item.done"), Some(index)) => {
                made.insert(index, Made::Done(&event["item"]));
            }
            (Some(kind), Some(index)) if DELTAS.contains(&kind) => {
                if let (Some(Made::Making { text, part, .. }), Some(delta)) =
                    (made.get_mut(&index), event["delta"].as_str())
                {
                    // A finished item's text is its parts' joined by line.
                    let this = (
                        event["summary_index"].as_u64(),
                        event["content_index"].as_u64(),
                    );
                    if part.is_some_and(|part| part != this) {
                        text.push('\n');
                    }
                    *part = Some(this);
                    text.push_str(delta);
                }
            }
            (Some(kind), _) if ENDS.contains(&kind) => end = Some(&event["response"]),
            _ => {}
        }
    }
    let output = end.and_then(|end| end["output"].as_array());
    let items = match output {
        Some(output) if !output.is_empty() => output.iter().map(entry).collect(),
        _ => made.into_values().map(made_entry).collect(),
    };
    (items, end.and_then(stop), end.is_some())
}

fn made_entry(made: Made) -> Item {
    let (added, delta) = match made {
        Made::Done(item) => return entry(item),
        Made::Making { added, text, .. } => (added, text),
    };
    let mut entry = entry(added);
    match &mut entry {
        Item::Assistant { text } | Item::Reasoning { text } => *text = delta,
        Item::Call { arguments, .. } => *arguments = delta,
        Item::System { .. } | Item::User { .. } | Item::Output { .. } | Item::Other { .. } => {}
    }
    entry
}

fn config(body: &Map<String, Value>) -> Config {
    let effort = body
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("effort"));
    config_of(body, effort, &["max_output_tokens"])
}
