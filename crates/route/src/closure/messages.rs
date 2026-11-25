//! The rules of the Messages API's entry.

use std::borrow::Cow;

use schema::ir::Api;
use serde_json::{Map, Value};

use super::{Endpoint, Refusal, Rules, bad, kept_state, shown};
use crate::{Apart, usage};

// `anthropic-version` is the version of the API that the client writes in.
const HEADERS: [&str; 3] = ["content-type", "accept", "anthropic-version"];

const RESPONSE_HEADERS: [&str; 3] = ["content-type", "request-id", "retry-after"];

pub(super) const RULES: Rules = Rules {
    // With the API's beta namespace, to which Anthropic's SDKs send a request through their beta
    // client: the features a beta turns on, the rules refuse by their fields.
    endpoint: Endpoint::Fixed(&["/v1/messages", "/v1/messages?beta=true"]),
    headers: &HEADERS,
    response_headers: &RESPONSE_HEADERS,
    apart: Apart {
        conversation: "messages",
        whole: &["tools", "system"],
    },
    usage: usage::messages,
    failure_code: "type",
    api: Api::Messages,
    stored: keeps_no_state,
    feature: picks_no_server_feature,
    tool: custom_tool,
    choice: chooses_declared_tool,
    object: fetches_nothing,
};

fn keeps_no_state(body: &Map<String, Value>) -> Result<(), Refusal> {
    let set = |field: &str| body.get(field).is_some_and(|value| !value.is_null());
    if set("container") {
        return Err(bad(
            "`container` names a container kept on the provider's side, which the route refuses: \
             leave it out, and run code through a custom tool",
        ));
    }
    if set("thread") {
        return Err(kept_state("thread"));
    }
    Ok(())
}

fn picks_no_server_feature(body: &Map<String, Value>) -> Result<(), Refusal> {
    let set = |field: &str| body.get(field).is_some_and(|value| !value.is_null());
    if set("mcp_servers") {
        return Err(bad(
            "`mcp_servers` has the provider call MCP servers, which the route refuses: leave it \
             out, and declare their tools as custom tools",
        ));
    }
    // `"default"` or a list of models, each run on the same request when the one before declines
    // it: https://platform.claude.com/docs/en/build-with-claude/refusals-and-fallback
    if set("fallbacks") {
        return Err(bad(
            "`fallbacks` has the provider answer with another model when the one named declines, \
             which the route refuses: leave it out, and name a model the route allows",
        ));
    }
    Ok(())
}

/// Refuses a tool of the Messages API's that is not custom: one the provider runs, such as its
/// web search, or one whose form it defines, such as its text editor.
// Even a tool whose calls come back to the client, such as `bash_20250124`, is one the provider
// gives a schema and a prompt of its own, which the workflow didn't choose.
fn custom_tool(tool: &Value) -> Result<(), Refusal> {
    let kind = match tool.get("type") {
        None => return Ok(()),
        Some(Value::String(kind)) if kind == "custom" => return Ok(()),
        Some(Value::String(kind)) => Cow::Borrowed(kind.as_str()),
        Some(other) => Cow::Owned(other.to_string()),
    };
    Err(bad(format!(
        "the tool `{}` is one the provider defines or runs, which the route refuses: declare \
         custom tools only, of no `type` or `custom`",
        shown(&kind)
    )))
}

fn chooses_declared_tool(body: &Map<String, Value>) -> Result<(), Refusal> {
    // A `tool` choice names one of the body's `tools`, which are custom tools alone.
    let Some(Value::Object(choice)) = body.get("tool_choice") else {
        return Ok(());
    };
    let kind = choice
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if matches!(kind, "auto" | "any" | "none" | "tool") {
        return Ok(());
    }
    Err(bad(format!(
        "the tool choice `{}` is none the route admits: choose `auto`, `any`, `none`, or a \
         `tool` that the request declares",
        shown(kind)
    )))
}

/// Refuses a content block of the Messages API's whose content the provider fetches: one whose
/// `source` is a URL or a file it stores, or a file uploaded to a container. A tool call's
/// `input` is the client's own data, which the provider never fetches.
fn fetches_nothing(object: &Map<String, Value>) -> Result<Option<&'static str>, Refusal> {
    let kind = object.get("type").and_then(Value::as_str);
    if kind == Some("container_upload") {
        return Err(bad(
            "a `container_upload` block puts a file in a container kept on the provider's side, \
             which the route refuses: send the content inline",
        ));
    }
    let source = object.get("source").and_then(|source| source.get("type"));
    if let Some(source @ ("url" | "file")) = source.and_then(Value::as_str) {
        return Err(bad(format!(
            "a block's `source` is a {source} that the provider would fetch, which the route \
             refuses: send the content inline, as `base64` or `text`"
        )));
    }
    Ok((kind == Some("tool_use")).then_some("input"))
}
