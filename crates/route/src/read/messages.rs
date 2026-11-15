//! The Messages API's forms: its stream's events, of which the reader knows where the output
//! begins. Its messages and answers are each of a form the reader doesn't know.

use serde_json::{Map, Value};

use super::{Dialect, Item, config_of};

pub(super) const DIALECT: Dialect = Dialect {
    output,
    item: |item| vec![Item::Other { json: item.clone() }],
    items,
    body: |_| (Vec::new(), None),
    stream: |_| (Vec::new(), None, false),
    config: |body| config_of(body, None, &["max_tokens"]),
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
            let said = match delta["type"].as_str() {
                Some("text_delta") => &delta["text"],
                Some("thinking_delta") => &delta["thinking"],
                Some("input_json_delta") => &delta["partial_json"],
                _ => return false,
            };
            said.as_str().is_some_and(|said| !said.is_empty())
        }
        Some("content_block_start") => event["content_block"]["type"] == "tool_use",
        _ => false,
    }
}

fn items(body: &Map<String, Value>) -> Vec<Item> {
    let messages = body.get("messages").and_then(Value::as_array);
    let messages = messages.into_iter().flatten();
    messages
        .map(|item| Item::Other { json: item.clone() })
        .collect()
}
