//! The Responses objects carried whole and by the events of a stream.
//!
//! <https://developers.openai.com/api/reference/resources/responses/methods/create>.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use crate::{Failure, Usage};

pub(crate) fn model_name<'a>(model: &'a str, upstream_name: &str, answered: &'a str) -> &'a str {
    if answered.eq_ignore_ascii_case(upstream_name) {
        model
    } else {
        answered
    }
}

pub(super) fn response(
    echo: &Value,
    id: &str,
    model: &str,
    created: u64,
    status: &str,
    output: Value,
    usage: Value,
) -> Value {
    let mut response = echo.clone();
    response["id"] = json!(id);
    response["object"] = json!("response");
    response["created_at"] = json!(created);
    response["model"] = json!(model);
    response["status"] = json!(status);
    response["output"] = output;
    response["error"] = Value::Null;
    response["incomplete_details"] = Value::Null;
    response["usage"] = usage;
    response
}

pub(super) fn usage(tokens: Option<Usage>) -> Value {
    let Some(tokens) = tokens else {
        return Value::Null;
    };
    json!({
        "input_tokens": tokens.input_tokens,
        "input_tokens_details": {
            "cached_tokens": tokens.cached_tokens.unwrap_or_default(),
            "cache_write_tokens": tokens.cache_write_tokens.unwrap_or_default(),
        },
        "output_tokens": tokens.output_tokens,
        "output_tokens_details": {"reasoning_tokens":tokens.reasoning_tokens.unwrap_or_default()},
        "total_tokens": tokens.input_tokens.saturating_add(tokens.output_tokens),
    })
}

/// The public error code drives client recovery. Free-form messages and unknown identifiers
/// can contain private provider diagnostics; keep those in the archive.
pub(crate) fn provider_error(error: &Value) -> Value {
    let code = error["code"].as_str().unwrap_or_default();
    let message = match code {
        "context_length_exceeded" => "the model request exceeds the context window",
        "invalid_prompt" | "invalid_request_error" => "the model provider rejected the request",
        "insufficient_quota"
        | "credit_balance_exhausted"
        | "organization_spend_limit_exceeded"
        | "project_spend_limit_exceeded"
        | "usage_not_included" => "the model provider's allocation is exhausted",
        "rate_limit_exceeded" | "slow_down" => "the model provider rate-limited the request",
        "server_error" | "server_is_overloaded" | "policy_capacity_unavailable" => {
            "the model provider could not serve the request"
        }
        "content_policy_violation"
        | "bio_policy"
        | "cyber_policy"
        | "misalignment_policy_violation" => {
            "the model provider refused the request under its policy"
        }
        _ => {
            return json!({"code":"server_error","message":"the model provider refused the Chat request"});
        }
    };
    json!({"code":code,"message":message})
}

pub(super) fn call_item_id(response_id: &str, output_index: usize) -> String {
    format!("fc_{output_index}_{response_id}")
}

pub(super) fn message(id: &str, status: &str, parts: Value) -> Value {
    let mut message = json!({"type":"message","id":id,"role":"assistant","status":status});
    message["content"] = parts;
    message
}

pub(super) fn call(id: &str, call_id: &str, name: &str, arguments: &str, status: &str) -> Value {
    json!({"type":"function_call","id":id,"call_id":call_id,"name":name,"arguments":arguments,"status":status})
}

/// A single choice as Responses output; a length or safety limit ends it incomplete.
pub(crate) fn whole(
    echo: &Value,
    model: &str,
    upstream_name: &str,
    chat: &Value,
    tokens: Option<Usage>,
) -> Option<(Value, Option<Failure>)> {
    if chat["object"] != "chat.completion" {
        return None;
    }
    let [choice] = chat["choices"].as_array()?.as_slice() else {
        return None;
    };
    if choice["index"] != 0 || choice["message"]["role"] != "assistant" {
        return None;
    }
    let id = chat["id"].as_str().filter(|id| !id.is_empty())?;
    let reason = match choice["finish_reason"].as_str()? {
        "stop" | "tool_calls" => None,
        "length" => Some("max_output_tokens"),
        "content_filter" => Some("content_filter"),
        _ => return None,
    };
    let status = if reason.is_some() {
        "incomplete"
    } else {
        "completed"
    };
    let output = output(id, status, choice)?;
    let model = model_name(model, upstream_name, chat["model"].as_str()?);
    let mut response = response(
        echo,
        id,
        model,
        chat["created"].as_u64()?,
        status,
        Value::Array(output),
        usage(tokens),
    );
    if let Some(reason) = reason {
        response["incomplete_details"] = json!({"reason":reason});
    }
    Some((response, reason.map(Failure::new)))
}

fn output(id: &str, status: &str, choice: &Value) -> Option<Vec<Value>> {
    let msg = &choice["message"];
    // Content that has no representation in the supported Responses output must stay visible
    // as an error instead of disappearing from a successful answer.
    let supported = msg.as_object()?.iter().all(|(field, value)| {
        value.is_null()
            || matches!(
                field.as_str(),
                "role" | "content" | "tool_calls" | "refusal"
            )
            || field == "annotations" && value.as_array().is_some_and(Vec::is_empty)
    });
    if !supported || !choice["logprobs"].is_null() {
        return None;
    }
    let mut parts = Vec::new();
    if !msg["content"].is_null() {
        parts.push(json!({"type":"output_text","text":msg["content"].as_str()?,"annotations":[]}));
    }
    if !msg["refusal"].is_null() {
        parts.push(json!({"type":"refusal","refusal":msg["refusal"].as_str()?}));
    }
    let mut output = Vec::new();
    if !parts.is_empty() {
        output.push(message(&format!("msg_{id}"), status, Value::Array(parts)));
    }
    let mut ids = BTreeSet::new();
    if !msg["tool_calls"].is_null() {
        for tool in msg["tool_calls"].as_array()? {
            let call_id = tool["id"].as_str()?;
            if tool["type"] != "function" || call_id.is_empty() || !ids.insert(call_id) {
                return None;
            }
            let function = &tool["function"];
            let name = function["name"].as_str()?;
            if name.is_empty() {
                return None;
            }
            output.push(call(
                &call_item_id(id, output.len()),
                call_id,
                name,
                function["arguments"].as_str()?,
                status,
            ));
        }
    }
    if choice["finish_reason"] == "tool_calls" && ids.is_empty()
        || output.is_empty() && status == "completed"
    {
        return None;
    }
    Some(output)
}
