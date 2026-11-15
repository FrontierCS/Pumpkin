//! The Chat Completions API's forms: its messages, and its stream's chunks.
//!
//! A message gives an entry for each of its reasoning, its text and its tool calls, each a
//! function's or a custom tool's. A stream's answer is a message for each choice, joined from its
//! chunks' deltas by the choice's index and each tool call's, as the API's own client joins them:
//! <https://github.com/openai/openai-python/blob/ea5d1606f441d80dca1e9e7f99b74581f41408f8/src/openai/lib/streaming/_deltas.py#L10-L40>.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::{Config, Dialect, Item, Streamed, config_of, string, text};

pub(super) const DIALECT: Dialect = Dialect {
    output,
    item,
    // Its API marks no item for one request alone.
    unmarked: Value::clone,
    items,
    body,
    stream,
    config,
};

/// The fields in which providers give a message's reasoning.
// `reasoning_content` is DeepSeek's and Z.ai's, `reasoning` OpenRouter's.
const REASONING: [&str; 2] = ["reasoning_content", "reasoning"];

/// A Chat Completions chunk that is output: one whose choice's delta holds content or reasoning
/// that is not empty, or a tool call that is named or passes something; a delta that only names
/// its role, or holds empty content, isn't.
fn output(event: &Value) -> bool {
    let said = |value: Option<&str>| value.is_some_and(|text| !text.is_empty());
    let choices = event["choices"].as_array().into_iter().flatten();
    choices.map(|choice| &choice["delta"]).any(|delta| {
        let calls = delta["tool_calls"].as_array().into_iter().flatten();
        let call = |call: &Value| {
            let called = called(call);
            called["name"].is_string() || said(passed(called))
        };
        ["content", REASONING[0], REASONING[1]]
            .into_iter()
            .any(|field| said(delta[field].as_str()))
            || calls.into_iter().any(call)
    })
}

/// The object of `call`, a tool call or a delta of one, that names its tool and holds what it
/// passes: the one its type names, `function` or `custom`, or, of a delta that gives no type,
/// whichever it holds.
fn called(call: &Value) -> &Value {
    let kind = (call["type"].as_str()).or_else(|| {
        ["function", "custom"]
            .into_iter()
            .find(|kind| call.get(kind).is_some())
    });
    &call[kind.unwrap_or("function")]
}

/// What `called` passes: a function's arguments, or a custom tool's input.
fn passed(called: &Value) -> Option<&str> {
    ["arguments", "input"]
        .into_iter()
        .find_map(|field| called[field].as_str())
}

/// The first of the fields in which providers give reasoning that `message`, a message or a
/// delta of one, holds.
fn reasoning(message: &Value) -> Option<&str> {
    REASONING.iter().find_map(|field| message[field].as_str())
}

fn item(message: &Value) -> Vec<Item> {
    let content = text(&message["content"]);
    match message["role"].as_str() {
        Some("system" | "developer") => vec![Item::System { text: content }],
        Some("user") => vec![Item::User { text: content }],
        Some("tool") => vec![Item::Output {
            call_id: string(message, "tool_call_id"),
            text: content,
            error: false,
        }],
        Some("assistant") => {
            let calls = message["tool_calls"].as_array().into_iter().flatten();
            let calls = calls.map(|call| {
                let called = called(call);
                Item::Call {
                    id: string(call, "id"),
                    name: string(called, "name").unwrap_or_default(),
                    arguments: passed(called).unwrap_or_default().to_owned(),
                }
            });
            let reasoning = reasoning(message).unwrap_or_default().to_owned();
            assistant(reasoning, content, calls.collect())
        }
        _ => vec![Item::Other {
            json: message.clone(),
        }],
    }
}

/// The entries of an assistant's message: its reasoning, if not empty; its text, if not empty or
/// if it holds nothing else; and its calls.
fn assistant(reasoning: String, content: String, calls: Vec<Item>) -> Vec<Item> {
    let mut items = Vec::new();
    if !reasoning.is_empty() {
        items.push(Item::Reasoning { text: reasoning });
    }
    if !content.is_empty() || (items.is_empty() && calls.is_empty()) {
        items.push(Item::Assistant { text: content });
    }
    items.extend(calls);
    items
}

fn items(body: &Map<String, Value>) -> Vec<Item> {
    let messages = body.get("messages").and_then(Value::as_array);
    messages.into_iter().flatten().flat_map(item).collect()
}

/// Each choice's message, and the first choice's reason to finish.
fn body(body: &Value) -> (Vec<Item>, Option<String>) {
    let choices = body["choices"].as_array().into_iter().flatten();
    let items = choices
        .clone()
        .flat_map(|choice| item(&choice["message"]))
        .collect();
    let stop = choices
        .into_iter()
        .find_map(|choice| string(choice, "finish_reason"));
    (items, stop)
}

/// A choice's message so far, joined from its deltas.
#[derive(Default)]
struct Joined {
    content: String,
    reasoning: String,
    /// Each tool call's id, name and what it passes, by its index.
    calls: BTreeMap<u64, [String; 3]>,
    finish: Option<String>,
}

/// A message of each choice; whole once each choice has given its reason to finish.
fn stream(events: &[Value]) -> Streamed {
    let mut joined: BTreeMap<u64, Joined> = BTreeMap::new();
    let choices = events
        .iter()
        .flat_map(|event| event["choices"].as_array().into_iter().flatten());
    for choice in choices {
        let index = choice["index"].as_u64().unwrap_or_default();
        let joined = joined.entry(index).or_default();
        let delta = &choice["delta"];
        joined.content += delta["content"].as_str().unwrap_or_default();
        joined.reasoning += reasoning(delta).unwrap_or_default();
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = call["index"].as_u64().unwrap_or_default();
            let [id, name, passes] = joined.calls.entry(index).or_default();
            let called = called(call);
            *id += call["id"].as_str().unwrap_or_default();
            *name += called["name"].as_str().unwrap_or_default();
            *passes += passed(called).unwrap_or_default();
        }
        if let Some(finish) = choice["finish_reason"].as_str() {
            joined.finish = Some(finish.to_owned());
        }
    }
    let whole = !joined.is_empty() && joined.values().all(|joined| joined.finish.is_some());
    let stop = joined.values().find_map(|joined| joined.finish.clone());
    let items = joined.into_values().flat_map(|joined| {
        let calls = joined
            .calls
            .into_values()
            .map(|[id, name, arguments]| Item::Call {
                id: (!id.is_empty()).then_some(id),
                name,
                arguments,
            });
        assistant(joined.reasoning, joined.content, calls.collect())
    });
    (items.collect(), stop, whole)
}

fn config(body: &Map<String, Value>) -> Config {
    config_of(
        body,
        body.get("reasoning_effort"),
        &["max_completion_tokens", "max_tokens"],
    )
}
