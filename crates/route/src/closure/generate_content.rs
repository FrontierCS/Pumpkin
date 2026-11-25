//! The rules of the Gemini API's entry: its `generateContent` and `streamGenerateContent`
//! methods, each at a path that names its model.

use schema::ir::Api;
use serde_json::{Map, Value};

use super::{Endpoint, Refusal, Rules, bad, closed_below, kept_state, shown};
use crate::{Apart, usage};

// The Gemini API's clients send it no header of its own but its key's.
const HEADERS: [&str; 2] = ["content-type", "accept"];

// The Gemini API gives no id for a request in a header.
const RESPONSE_HEADERS: [&str; 2] = ["content-type", "retry-after"];

/// The two spellings of a field of the Gemini API's, either of which the API reads: in camel
/// case, as its documentation writes it, and as its protocol buffer names it.
// https://protobuf.dev/programming-guides/json/ has a parser of ProtoJSON take both.
type Spelled = [&'static str; 2];

const CACHED_CONTENT: Spelled = ["cachedContent", "cached_content"];
const FILE_URI: Spelled = ["fileUri", "file_uri"];
const FUNCTION_CALL: Spelled = ["functionCall", "function_call"];
const FUNCTION_DECLARATIONS: Spelled = ["functionDeclarations", "function_declarations"];
const FUNCTION_RESPONSE: Spelled = ["functionResponse", "function_response"];

pub(super) const RULES: Rules = Rules {
    endpoint: Endpoint::Models {
        prefix: "/v1beta/models/",
        // A stream without `alt=sse` is one JSON array, whose usage no reader of events finds.
        methods: &[
            ("generateContent", None, false),
            ("streamGenerateContent", Some("alt=sse"), true),
        ],
    },
    headers: &HEADERS,
    response_headers: &RESPONSE_HEADERS,
    apart: Apart {
        conversation: "contents",
        whole: &["tools", "systemInstruction"],
    },
    usage: usage::generate_content,
    failure_code: "status",
    api: Api::GenerateContent,
    stored: caches_nothing,
    feature: names_no_model,
    tool: declares_functions,
    choice: |_| Ok(()),
    object: fetches_no_file,
};

/// The field of `object` that one of `spelled` names and that holds a value, with its name.
fn spelled(object: &Map<String, Value>, spelled: Spelled) -> Option<(&'static str, &Value)> {
    spelled
        .into_iter()
        .find_map(|name| Some((name, object.get(name).filter(|value| !value.is_null())?)))
}

fn caches_nothing(body: &Map<String, Value>) -> Result<(), Refusal> {
    match spelled(body, CACHED_CONTENT) {
        Some((field, _)) => Err(kept_state(field)),
        None => Ok(()),
    }
}

fn names_no_model(body: &Map<String, Value>) -> Result<(), Refusal> {
    if body.get("model").is_some_and(|value| !value.is_null()) {
        return Err(bad(
            "`model` is in the body, where it could name another model than the path: leave it \
             out",
        ));
    }
    Ok(())
}

/// Refuses a tool of the Gemini API's that holds anything but function declarations: one the
/// provider runs, such as its search, or one whose form it defines, such as its computer use.
fn declares_functions(tool: &Value) -> Result<(), Refusal> {
    let fields = tool.as_object().into_iter().flatten();
    let other = fields
        .filter(|(_, value)| !value.is_null())
        .find(|(field, _)| !FUNCTION_DECLARATIONS.contains(&field.as_str()));
    let Some((other, _)) = other else {
        return Ok(());
    };
    Err(bad(format!(
        "the tool `{}` is one the provider defines or runs, which the route refuses: declare \
         functions only, in `functionDeclarations`",
        shown(other)
    )))
}

/// Refuses a file part's URI, a string that names a file for the provider to fetch. A field of
/// another type names nothing, such as a function's parameter that its schema calls `file_uri`.
/// A function call is the client's own data, and so is a function response's `response`: of a
/// response, only its `parts`, which hold media as a content's do, are checked.
fn fetches_no_file(object: &Map<String, Value>) -> Result<Option<&'static str>, Refusal> {
    if let Some((field, Value::String(_))) = spelled(object, FILE_URI) {
        return Err(bad(format!(
            "`{field}` names a file that the provider would fetch, which the route refuses: send \
             the content inline, as `inlineData`"
        )));
    }
    if let Some((field, _)) = spelled(object, FUNCTION_CALL) {
        return Ok(Some(field));
    }
    let Some((field, response)) = spelled(object, FUNCTION_RESPONSE) else {
        return Ok(None);
    };
    if let Some(parts) = response.get("parts") {
        closed_below(&RULES, parts)?;
    }
    Ok(Some(field))
}
