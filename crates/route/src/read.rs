//! Reading a kept model exchange, of either API, into what any API's conversation holds: each of
//! its items as one kind of [`Item`].
//!
//! A request's body is one as it went upstream, which a record's reader rebuilds from the items the
//! record keeps apart. A kept request file holds digests in place of its tools and instructions,
//! which this reader would read as no tools and as text.
//!
//! What a body holds is the provider's, so reading it never fails: an item of a form that the
//! reader doesn't know is [`Item::Other`], and an event it doesn't know is passed over. Each API's
//! forms are its own module's; what the reader makes of them is the same for every API.

mod chat;
mod responses;

use schema::ir::Api;
use serde_json::{Map, Value};

use crate::usage;

/// An item of a conversation, or of an answer, as one kind of entry, the same for every API.
///
/// A text is the item's parts' text joined by line, with a part of no text, such as an image, as
/// its type in brackets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// The system's or the developer's instructions.
    System {
        text: String,
    },
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    /// The model's reasoning, as far as the provider shows it: empty where it keeps it encrypted.
    Reasoning {
        text: String,
    },
    /// A tool call, which the tool's [`Item::Output`] names by `id`: of a function, with its
    /// arguments, or of a custom tool, with its input as `arguments`.
    Call {
        id: Option<String>,
        name: String,
        arguments: String,
    },
    /// The output of the call that `call_id` names.
    Output {
        call_id: Option<String>,
        text: String,
    },
    /// Of a form the reader doesn't know: the provider's JSON of it, as sent or as a stream added
    /// it.
    Other {
        json: Value,
    },
}

/// An answer's body, read as far as it was kept.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    /// The answer's items, in order: of a stream that has not ended, those it has made so far.
    pub items: Vec<Item>,
    /// Why the model stopped, in its API's words, if the answer says.
    pub stop: Option<String>,
    /// The byte at which each event of a stream begins, in order: its `data:` line's first.
    pub events: Vec<usize>,
    /// Where the stream's output begins, if it has: the byte after the end of the line of its
    /// first event that its API counts as output, which is where the route's write path tells
    /// [`Keep::first_output`](crate::Keep::first_output).
    pub first_output: Option<usize>,
    /// Whether the answer ended as its API ends one: a body that is one JSON value, or a stream
    /// that reached its last event.
    pub whole: bool,
}

/// What a request asked of the model, as far as its body says.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub temperature: Option<f64>,
    /// The most tokens that the answer may hold, its reasoning's included.
    pub max_tokens: Option<u64>,
    /// The sequences that stop the answer.
    pub stop: Vec<String>,
    pub stream: bool,
    /// The names of the tools it declares, or the type of one that has none.
    pub tools: Vec<String>,
}

/// What one API's reader knows of its forms.
pub(crate) struct Dialect {
    /// Whether an event of its stream is the model's output: the first that is marks the
    /// stream's first token, for the route as it passes and for the reader alike.
    // As deepseek-harness's `isTokenDelta` takes a chunk for the first token: a delta of text,
    // reasoning or a call's arguments that is not empty, or a call's delta that names it,
    // https://github.com/deepseek-ai/deepseek-harness/blob/39163dd30f703a79079e2d2446545cff3c453738/packages/llm/llm/src/assistant-stream.ts#L242-L258
    pub(crate) output: usage::Output,
    /// The entries of one item of a body's conversation.
    item: fn(&Value) -> Vec<Item>,
    /// The entries of a request's body, its conversation's and those of the fields that the
    /// API reads as items of it.
    items: fn(&Map<String, Value>) -> Vec<Item>,
    /// The items of an answer that is one JSON value, and why it stopped.
    body: fn(&Value) -> (Vec<Item>, Option<String>),
    stream: fn(&[Value]) -> Streamed,
    config: fn(&Map<String, Value>) -> Config,
}

/// The items of a stream's events, why it stopped, and whether it ended.
type Streamed = (Vec<Item>, Option<String>, bool);

pub(crate) fn dialect(api: Api) -> &'static Dialect {
    match api {
        Api::Responses => &responses::DIALECT,
        Api::Chat => &chat::DIALECT,
    }
}

/// The entries of `item`, one item of a request's conversation as `api` writes it, as a record
/// keeps it apart.
pub fn item(api: Api, item: &Value) -> Vec<Item> {
    (dialect(api).item)(item)
}

/// The entries of `body`, a request's body as it went upstream, in order.
///
/// Those of its conversation come after those of the fields that `api` reads as its first items,
/// such as the Responses API's `instructions`. There are none if `body` is no JSON object.
pub fn items(api: Api, body: &Value) -> Vec<Item> {
    body.as_object()
        .map(|body| (dialect(api).items)(body))
        .unwrap_or_default()
}

/// The answer that `response` holds, the bytes of a response's body as they came, or as many of
/// them as were kept: one JSON value, or a stream, whose last line is left out if it is torn.
pub fn answer(api: Api, response: &[u8]) -> Reply {
    let dialect = dialect(api);
    if let Ok(body) = serde_json::from_slice::<Value>(response) {
        let (items, stop) = (dialect.body)(&body);
        return Reply {
            items,
            stop,
            events: Vec::new(),
            first_output: None,
            whole: true,
        };
    }
    let mut events = Vec::new();
    let mut offsets = Vec::new();
    let mut first_output = None;
    let mut at = 0;
    for line in response.split_inclusive(|&byte| byte == b'\n') {
        let start = at;
        at += line.len();
        let Some(line) = line.strip_suffix(b"\n") else {
            break;
        };
        let Some(event) = usage::event(line) else {
            continue;
        };
        if first_output.is_none() && (dialect.output)(&event) {
            first_output = Some(at);
        }
        offsets.push(start);
        events.push(event);
    }
    let (items, stop, whole) = (dialect.stream)(&events);
    Reply {
        items,
        stop,
        events: offsets,
        first_output,
        whole,
    }
}

/// What `body`, a request's body as it went upstream, asked of the model.
pub fn config(api: Api, body: &Value) -> Config {
    let empty = Map::new();
    (dialect(api).config)(body.as_object().unwrap_or(&empty))
}

/// The field of a request's body, as `api` writes it, that holds its list of tools.
pub fn tools_field(api: Api) -> &'static str {
    match api {
        Api::Responses | Api::Chat => "tools",
    }
}

/// The text of `content`: a string, or its parts' text.
fn text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => joined(parts),
        Value::Object(_) => content.to_string(),
        Value::Null | Value::Bool(_) | Value::Number(_) => String::new(),
    }
}

/// The text of `parts`, joined by line: each a string, a part's text or refusal, or the text of
/// its output, else its type in brackets.
fn joined<'a>(parts: impl IntoIterator<Item = &'a Value>) -> String {
    let parts = parts.into_iter().map(|part| match part {
        Value::String(text) => text.clone(),
        _ => match (string(part, "text"), string(part, "refusal")) {
            (Some(text), _) | (None, Some(text)) => text,
            (None, None) => match part.get("output") {
                Some(output) => text(output),
                None => format!("[{}]", string(part, "type").unwrap_or_default()),
            },
        },
    });
    parts.collect::<Vec<_>>().join("\n")
}

fn string(value: &Value, field: &str) -> Option<String> {
    value.get(field)?.as_str().map(str::to_owned)
}

/// The names of the tools that `tools` declares: each one's `name`, or that of the object its
/// type names, as a Chat Completions `function` tool gives it, else its type.
fn tool_names(tools: Option<&Value>) -> Vec<String> {
    let tools = tools.and_then(Value::as_array).into_iter().flatten();
    let name = |tool: &Value| {
        let kind = string(tool, "type").unwrap_or_default();
        (tool.get(&kind).and_then(|named| string(named, "name")))
            .or_else(|| string(tool, "name"))
            .unwrap_or(kind)
    };
    tools.map(name).collect()
}

/// The config that `body` gives, with the fields that its API names for the reasoning's effort
/// and the answer's most tokens.
fn config_of(body: &Map<String, Value>, effort: Option<&Value>, max_tokens: &[&str]) -> Config {
    let stop = match body.get("stop") {
        Some(Value::String(stop)) => vec![stop.clone()],
        Some(Value::Array(stops)) => stops
            .iter()
            .filter_map(|stop| stop.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    };
    Config {
        model: body.get("model").and_then(Value::as_str).map(str::to_owned),
        reasoning_effort: effort.and_then(Value::as_str).map(str::to_owned),
        temperature: body.get("temperature").and_then(Value::as_f64),
        max_tokens: (max_tokens.iter()).find_map(|field| body.get(*field)?.as_u64()),
        stop,
        stream: body.get("stream").and_then(Value::as_bool) == Some(true),
        tools: tool_names(body.get("tools")),
    }
}
