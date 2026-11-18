//! Self-contained Responses requests whose instructions, text and function tools Chat can carry.

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use super::Request;
use crate::RequestRepair;

type Object = Map<String, Value>;

/// Makes an `EasyInputMessage` explicit before the closure rejects untyped item references.
pub(crate) fn explicit_messages(body: &mut Object) {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    for item in items {
        if let Some(object) = item.as_object_mut()
            && object.len() == 2
            && object.contains_key("content")
            && matches!(
                object.get("role").and_then(Value::as_str),
                Some("user" | "assistant" | "developer" | "system")
            )
        {
            object.insert("type".into(), json!("message"));
        }
    }
}

pub(crate) fn request(
    mut body: Object,
    repair: Option<RequestRepair>,
) -> Result<(Object, Request), String> {
    let mut messages = Vec::new();
    let mut echo =
        json!({"tools": [], "tool_choice": "auto", "parallel_tool_calls": true, "store": false});
    if let Some(instructions) = body.remove("instructions").filter(|v| !v.is_null()) {
        let text = instructions.as_str().ok_or("send instructions as text")?;
        messages.push(json!({"role":"developer","content":text}));
        echo["instructions"] = instructions;
    }
    let input = body
        .remove("input")
        .ok_or("send input as text or a list of items")?;
    let repair = history(input, &mut messages, repair)?;
    let mut chat = Map::from_iter([("messages".to_owned(), Value::Array(messages))]);
    let stream = match body.remove("stream") {
        None => false,
        Some(value) => {
            let stream = value.as_bool().ok_or("send stream as a boolean")?;
            chat.insert("stream".into(), value);
            if stream {
                chat.insert("stream_options".into(), json!({"include_usage":true}));
            }
            stream
        }
    };
    for (field, value) in body {
        match field.as_str() {
            "model" | "store" => {
                chat.insert(field, value);
            }
            // This selects a field of a reasoning output item; this bridge produces none.
            "include"
                if value.as_array().is_some_and(|items| {
                    items.iter().all(|v| v == "reasoning.encrypted_content")
                }) => {}
            "client_metadata" => telemetry(&value)?,
            "previous_response_id" if value.is_null() => {}
            _ => {
                parameter(&field, &value, &mut chat)?;
                echo[&field] = value;
            }
        }
    }
    Ok((
        chat,
        Request {
            repair,
            stream,
            echo,
        },
    ))
}

fn parameter(field: &str, value: &Value, chat: &mut Object) -> Result<(), String> {
    let key = match field {
        "temperature" | "top_p" if value.is_number() => field,
        "parallel_tool_calls" if value.is_boolean() => field,
        "max_output_tokens" if value.as_u64().is_some() => "max_completion_tokens",
        "prompt_cache_key" if value.is_string() => field,
        "metadata"
            if value
                .as_object()
                .is_some_and(|map| map.values().all(Value::is_string)) =>
        {
            field
        }
        "truncation" if value == "disabled" => return Ok(()),
        "text" | "reasoning" if value.is_null() => return Ok(()),
        "text" => {
            fields(value, &["verbosity", "format"])?;
            if let Some(format) = value.get("format") {
                if format != &json!({"type":"text"}) {
                    return Err(unsupported("text.format"));
                }
                chat.insert("response_format".into(), format.clone());
            }
            return setting(
                value,
                "verbosity",
                &["low", "medium", "high"],
                chat,
                "verbosity",
            );
        }
        "reasoning" => {
            fields(value, &["effort"])?;
            return setting(
                value,
                "effort",
                &["none", "minimal", "low", "medium", "high", "xhigh", "max"],
                chat,
                "reasoning_effort",
            );
        }
        "tools" => {
            let tools = value
                .as_array()
                .ok_or("send tools as a list of functions")?;
            let functions = tools.iter().map(function).collect::<Result<Vec<_>, _>>()?;
            chat.insert(field.into(), Value::Array(functions));
            return Ok(());
        }
        "tool_choice" if matches!(value.as_str(), Some("none" | "auto" | "required")) => field,
        "tool_choice" if value["type"] == "function" => {
            fields(value, &["name", "type"])?;
            chat.insert(
                field.into(),
                json!({"type":"function","function":{"name":string(value,"name")?}}),
            );
            return Ok(());
        }
        _ => return Err(unsupported(field)),
    };
    chat.insert(key.into(), value.clone());
    Ok(())
}

fn setting(
    value: &Value,
    field: &str,
    allowed: &[&str],
    chat: &mut Object,
    key: &str,
) -> Result<(), String> {
    if let Some(setting) = value.get(field) {
        if !setting.as_str().is_some_and(|s| allowed.contains(&s)) {
            return Err(unsupported(field));
        }
        chat.insert(key.into(), setting.clone());
    }
    Ok(())
}

// Codex's client telemetry is carried by Responses alone. Keep it off the provider connection.
// https://github.com/openai/codex/blob/086ccc833bc9cf01024ad2dd6af7dbd421b6bf9a/codex-rs/core/src/responses_metadata.rs#L338-L393
fn telemetry(value: &Value) -> Result<(), String> {
    fields(
        value,
        &[
            "x-codex-installation-id",
            "session_id",
            "thread_id",
            "x-codex-window-id",
            "turn_id",
            "x-openai-subagent",
            "x-codex-parent-thread-id",
            "parent_turn_id",
            "root_turn_id",
            "x-codex-turn-metadata",
        ],
    )?;
    if value
        .as_object()
        .is_some_and(|fields| fields.values().all(Value::is_string))
    {
        Ok(())
    } else {
        Err("send Codex client metadata as strings".into())
    }
}

fn unsupported(field: &str) -> String {
    let field = &field[..field.floor_char_boundary(field.len().min(100))];
    format!(
        "Chat translation cannot carry `{}`: omit it or use a native Responses model",
        field.escape_debug()
    )
}

fn fields(value: &Value, allowed: &[&str]) -> Result<(), String> {
    let object = value.as_object().ok_or("send an object")?;
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(unsupported(field));
    }
    Ok(())
}

fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str, String> {
    value[field]
        .as_str()
        .ok_or_else(|| format!("send `{field}` as a string"))
}

fn function(tool: &Value) -> Result<Value, String> {
    fields(
        tool,
        &["type", "name", "description", "parameters", "strict"],
    )?;
    if tool["type"] != "function" {
        return Err("Chat translation takes function tools only".into());
    }
    string(tool, "name")?;
    if tool.get("description").is_some_and(|v| !v.is_string())
        || tool.get("parameters").is_some_and(|v| !v.is_object())
    {
        return Err(
            "send a function description as text and its parameters as a schema object".into(),
        );
    }
    // Responses may normalize a schema to strict mode when this is omitted; Chat defaults to false.
    if !tool["strict"].is_boolean() {
        return Err("set each function's strict to true or false for Chat translation".into());
    }
    let mut function = tool.clone();
    if let Some(object) = function.as_object_mut() {
        object.remove("type");
    }
    Ok(json!({"type":"function","function":function}))
}

fn history(
    input: Value,
    messages: &mut Vec<Value>,
    repair: Option<RequestRepair>,
) -> Result<Option<RequestRepair>, String> {
    let items = match input {
        Value::String(text) => {
            messages.push(json!({"role":"user","content":text}));
            return Ok(None);
        }
        Value::Array(items) => items,
        _ => return Err("send input as text or a list of items".into()),
    };
    let mut pending = BTreeSet::new();
    let mut applied = None;
    let mut items = items.into_iter().peekable();
    while let Some(item) = items.next() {
        match item["type"].as_str() {
            Some("message") => {
                let turn = if repair == Some(RequestRepair::JoinAssistantToolText)
                    && item["role"] == "assistant"
                    && items
                        .peek()
                        .is_some_and(|next| next["type"] == "function_call_output")
                {
                    messages
                        .last_mut()
                        .filter(|last| last["tool_calls"].is_array())
                } else {
                    None
                };
                if turn.is_none() {
                    answered(&pending)?;
                }
                fields(&item, &["type", "id", "status", "role", "content"])?;
                let role = string(&item, "role")?;
                if !matches!(role, "developer" | "system" | "user" | "assistant") {
                    return Err("send a developer, system, user or assistant message".into());
                }
                completed(&item)?;
                let content = content(&item["content"], role == "assistant")?;
                if let Some(turn) = turn {
                    join_text(&mut turn["content"], content)?;
                    applied = repair;
                } else {
                    messages.push(json!({"role":role,"content":content}));
                }
            }
            Some("function_call") => {
                add_call(&item, messages, &mut pending)?;
            }
            Some("function_call_output") => {
                fields(&item, &["type", "id", "call_id", "status", "output"])?;
                completed(&item)?;
                let id = string(&item, "call_id")?;
                if !pending.remove(id) {
                    return Err(
                        "each function result must name a pending call_id exactly once".into(),
                    );
                }
                let output = content(&item["output"], false)?;
                messages.push(json!({"role":"tool","tool_call_id":id,"content":output}));
            }
            Some(kind) => return Err(unsupported(kind)),
            None => return Err("send each input item with its type".into()),
        }
    }
    answered(&pending)?;
    Ok(applied)
}

// LiteLLM keeps trailing assistant text with the calls whose results follow it.
// https://github.com/BerriAI/litellm/blob/df78b16941697991a736fc224182fa17270420ed/litellm/responses/litellm_completion_transformation/transformation.py#L1854-L1895
fn join_text(before: &mut Value, after: Value) -> Result<(), String> {
    let text_only = |value: &Value| {
        value.is_string()
            || value.as_array().is_some_and(|parts| {
                !parts.is_empty() && parts.iter().all(|part| part["type"] == "text")
            })
    };
    if !text_only(&after) || (!before.is_null() && !text_only(before)) {
        return Err("join_assistant_tool_text requires text on the assistant turn".into());
    }
    if before.is_null() {
        *before = after;
    } else {
        let parts = |content| match content {
            Value::Array(parts) => parts,
            text => vec![json!({"type":"text","text":text})],
        };
        let mut joined = parts(before.take());
        joined.extend(parts(after));
        *before = Value::Array(joined);
    }
    Ok(())
}

fn add_call(
    item: &Value,
    messages: &mut Vec<Value>,
    pending: &mut BTreeSet<String>,
) -> Result<(), String> {
    fields(
        item,
        &["type", "id", "call_id", "status", "name", "arguments"],
    )?;
    completed(item)?;
    let id = string(item, "call_id")?;
    if id.is_empty() || !pending.insert(id.to_owned()) {
        return Err("give each pending function call a distinct, nonempty call_id".into());
    }
    if messages
        .last()
        .is_some_and(|message| message["role"] == "tool")
        && pending.len() > 1
    {
        return Err("send all results for the previous assistant turn before another call".into());
    }
    let call = json!({"type":"function","id":id,"function":{
        "name":string(item,"name")?, "arguments":string(item,"arguments")?
    }});
    if let Some(assistant) = messages
        .last_mut()
        .filter(|last| last["role"] == "assistant")
    {
        if let Some(calls) = assistant["tool_calls"].as_array_mut() {
            calls.push(call);
        } else {
            assistant["tool_calls"] = json!([call]);
        }
    } else {
        messages.push(json!({"role":"assistant","content":null,"tool_calls":[call]}));
    }
    Ok(())
}

fn answered(pending: &BTreeSet<String>) -> Result<(), String> {
    if pending.is_empty() {
        Ok(())
    } else {
        Err("send all matching function results immediately after the assistant's calls".into())
    }
}

fn completed(item: &Value) -> Result<(), String> {
    if item.get("status").is_some_and(|s| s != "completed") {
        return Err("send only completed history items for Chat translation".into());
    }
    Ok(())
}

fn content(value: &Value, assistant: bool) -> Result<Value, String> {
    if value.is_string() {
        return Ok(value.clone());
    }
    let parts = value
        .as_array()
        .ok_or("send message content or function output as text or text parts")?;
    let parts = parts
        .iter()
        .map(|part| text_part(part, assistant))
        .collect::<Result<_, _>>()?;
    Ok(Value::Array(parts))
}

fn text_part(part: &Value, assistant: bool) -> Result<Value, String> {
    match part["type"].as_str() {
        Some("input_text") => fields(part, &["type", "text"])?,
        Some("refusal") if assistant => {
            fields(part, &["type", "refusal"])?;
            return Ok(json!({"type":"refusal","refusal":string(part,"refusal")?}));
        }
        Some("output_text") if assistant => {
            fields(part, &["type", "text", "annotations", "logprobs"])?;
            let carries_data = |field: &str| {
                let v = &part[field];
                !v.is_null() && !v.as_array().is_some_and(Vec::is_empty)
            };
            if ["annotations", "logprobs"].into_iter().any(carries_data) {
                return Err(
                    "Chat translation cannot carry annotations or log probabilities in history"
                        .into(),
                );
            }
        }
        Some(other) => return Err(unsupported(other)),
        None => return Err("send text parts with their type".into()),
    }
    Ok(json!({"type":"text","text":string(part,"text")?}))
}
