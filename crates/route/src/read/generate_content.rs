//! The Gemini API's forms: its contents' parts, and its stream's chunks.
//!
//! A content's parts are each an entry: a text by the content's role, a thought, a function call,
//! or a function's response. Parts of one kind that follow one another are one entry, as a stream
//! gives a text in several. A stream's answer is each candidate's content, its parts those of its
//! chunks in order, as the API's documentation has a client join them:
//! <https://ai.google.dev/api/generate-content#method:-models.streamgeneratecontent>.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::{Config, Dialect, Item, Streamed, string};

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

/// A chunk that is output: one whose candidate's content holds a text that is not empty, a
/// thought's included, or a function call.
fn output(event: &Value) -> bool {
    let candidates = event["candidates"].as_array().into_iter().flatten();
    candidates
        .flat_map(|candidate| {
            candidate["content"]["parts"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .any(|part| {
            part["text"].as_str().is_some_and(|text| !text.is_empty())
                || part.get("functionCall").is_some()
        })
}

fn item(content: &Value) -> Vec<Item> {
    let Some(parts) = content["parts"].as_array() else {
        return vec![Item::Other {
            json: content.clone(),
        }];
    };
    let mut items: Vec<Item> = Vec::new();
    for part in parts {
        let Some(next) = entry(content["role"].as_str(), part) else {
            continue;
        };
        match (items.last_mut(), next) {
            (Some(Item::User { text }), Item::User { text: more })
            | (Some(Item::Assistant { text }), Item::Assistant { text: more })
            | (Some(Item::Reasoning { text }), Item::Reasoning { text: more }) => *text += &more,
            (_, next) => items.push(next),
        }
    }
    items
}

/// The entry of `part`, one part of a content of `role`: a text or a thought, a call, a
/// response, or a part of no text, such as inline data, as its field's name in brackets. A part
/// that holds nothing, or a thought's signature alone, as a stream's last may, is no entry.
fn entry(role: Option<&str>, part: &Value) -> Option<Item> {
    if let Some(call) = part.get("functionCall") {
        return Some(Item::Call {
            id: string(call, "id"),
            name: string(call, "name").unwrap_or_default(),
            arguments: call.get("args").map(Value::to_string).unwrap_or_default(),
        });
    }
    if let Some(response) = part.get("functionResponse") {
        return Some(Item::Output {
            call_id: string(response, "id"),
            text: (response.get("response").map(Value::to_string)).unwrap_or_default(),
            error: false,
        });
    }
    let text = string(part, "text").or_else(|| {
        let field = (part.as_object().into_iter().flatten())
            .map(|(field, _)| field.as_str())
            .find(|field| !matches!(*field, "thought" | "thoughtSignature"));
        field.map(|field| format!("[{field}]"))
    })?;
    Some(match role {
        Some("model") if part["thought"] == true => Item::Reasoning { text },
        Some("model") => Item::Assistant { text },
        Some("user") => Item::User { text },
        _ => Item::Other { json: part.clone() },
    })
}

fn items(body: &Map<String, Value>) -> Vec<Item> {
    let system = (body.get("systemInstruction")).and_then(|system| system["parts"].as_array());
    let system = system.map(|parts| {
        let texts = parts.iter().filter_map(|part| part["text"].as_str());
        Item::System {
            text: texts.collect::<Vec<_>>().join("\n"),
        }
    });
    let contents = body.get("contents").and_then(Value::as_array);
    let contents = contents.into_iter().flatten().flat_map(item);
    system.into_iter().chain(contents).collect()
}

/// Each candidate's content, and the first candidate's reason to finish.
fn body(body: &Value) -> (Vec<Item>, Option<String>) {
    let candidates = body["candidates"].as_array().into_iter().flatten();
    let items = candidates
        .clone()
        .flat_map(|candidate| item(&candidate["content"]))
        .collect();
    let stop = candidates
        .into_iter()
        .find_map(|candidate| string(candidate, "finishReason"));
    (items, stop)
}

/// The content of each candidate, its parts those of its chunks; whole once each candidate has
/// given its reason to finish.
fn stream(events: &[Value]) -> Streamed {
    let mut joined: BTreeMap<u64, (Vec<Value>, Option<String>)> = BTreeMap::new();
    let candidates = events
        .iter()
        .flat_map(|event| event["candidates"].as_array().into_iter().flatten());
    for candidate in candidates {
        let index = candidate["index"].as_u64().unwrap_or_default();
        let (parts, finish) = joined.entry(index).or_default();
        parts.extend(
            candidate["content"]["parts"]
                .as_array()
                .into_iter()
                .flatten()
                .cloned(),
        );
        if let Some(reason) = candidate["finishReason"].as_str() {
            *finish = Some(reason.to_owned());
        }
    }
    let whole = !joined.is_empty() && joined.values().all(|(_, finish)| finish.is_some());
    let stop = joined.values().find_map(|(_, finish)| finish.clone());
    let items = joined.into_values().flat_map(|(parts, _)| {
        let mut content = Map::new();
        content.insert("role".to_owned(), Value::from("model"));
        content.insert("parts".to_owned(), Value::Array(parts));
        item(&Value::Object(content))
    });
    (items.collect(), stop, whole)
}

/// What the body's `generationConfig` asks: the path names the model and whether the answer is
/// a stream, so the body says neither.
fn config(body: &Map<String, Value>) -> Config {
    let generation = body.get("generationConfig");
    let asked = |field: &str| generation.and_then(|generation| generation.get(field));
    let thinking = asked("thinkingConfig");
    // A level, or else a budget in tokens.
    let effort = thinking.and_then(|thinking| {
        string(thinking, "thinkingLevel")
            .or_else(|| Some(thinking.get("thinkingBudget")?.as_i64()?.to_string()))
    });
    let stops = asked("stopSequences").and_then(Value::as_array);
    let stops = stops.into_iter().flatten().filter_map(Value::as_str);
    let tools = body.get("tools").and_then(Value::as_array);
    let declared = tools.into_iter().flatten().flat_map(|tool| {
        tool["functionDeclarations"]
            .as_array()
            .into_iter()
            .flatten()
    });
    Config {
        model: None,
        reasoning_effort: effort,
        temperature: asked("temperature").and_then(Value::as_f64),
        max_tokens: asked("maxOutputTokens").and_then(Value::as_u64),
        stop: stops.map(str::to_owned).collect(),
        stream: None,
        tools: declared
            .filter_map(|function| string(function, "name"))
            .collect(),
    }
}
