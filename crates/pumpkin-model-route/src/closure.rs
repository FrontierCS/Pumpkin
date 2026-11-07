//! Parsing a sandbox's model request into one that keeps the route closed, by the rules that
//! [the route's docs](super) give.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io;

use pumpkin_gateway::{IncomingModelRequest, ModelRequest};
use pumpkin_schema::ir::ModelName;
use serde_json::{Map, Value, json};
use stdx::Deadline;
use ureq::http::{HeaderName, HeaderValue};

use super::upstream::Answer;

/// The request headers that pass upstream.
// Each is one the harness the runs use sends, and describes its client, its session, or the form
// in which its requests are written. Any other could reach more of the provider than the models
// the route allows, under the deployment's key: a credential of the agent's own, or a header that
// picks the account, the project, a model or a feature. An unlisted header stays behind without a
// refusal, so the list names each header whole, and passes none for sharing a prefix with one.
//
// Two name a form of the request rather than describe its client: `x-codex-beta-features` names
// the form of compaction the harness's requests use, and `x-openai-internal-codex-responses-lite`
// the form that lists the tools in an `additional_tools` input item rather than in `tools`. Both
// pass as sent, since the harness writes its requests in the forms they name, and the rules read
// a body in any form: they find every list of tools wherever it sits.
const PASSED_HEADERS: [&str; 10] = [
    "content-type",
    "accept",
    "originator",
    "user-agent",
    "session-id",
    "thread-id",
    "x-codex-beta-features",
    "x-codex-window-id",
    "x-codex-turn-metadata",
    "x-openai-internal-codex-responses-lite",
];

/// The fields that refer to state kept on the provider's side.
const STATE_FIELDS: [&str; 4] = [
    "previous_response_id",
    "conversation",
    "prompt",
    "background",
];

/// The fields whose URL the provider would fetch.
const URL_FIELDS: [&str; 4] = ["url", "image_url", "file_url", "audio_url"];

/// The most bytes of a value the client chose, such as a path or a model's name, that
/// [`shown`] keeps.
const MAX_SHOWN_BYTES: usize = 256;

/// A role's model route, as the host serves it.
#[derive(Debug)]
pub struct Route {
    /// Each model that requests may name, with the name that its upstream takes for it.
    models: BTreeMap<ModelName, ModelName>,
}

/// A model request that keeps the route closed, ready to go upstream. Only [`Route::close`]
/// makes one.
#[derive(Debug)]
pub(crate) struct Closed {
    headers: Vec<(HeaderName, HeaderValue)>,
    body: Vec<u8>,
    /// The model that the request named, and the name that its upstream takes for it, which
    /// `body` names.
    model: (ModelName, ModelName),
}

/// Why the route answers a model request with an error, which [`Refusal::answer`] tells its
/// client: a rule broken, the route's end, or an upstream's answer missing or not passed on.
#[derive(Debug)]
pub(crate) struct Refusal {
    status: Status,
    message: String,
}

/// The HTTP status of a refusal.
#[derive(Clone, Copy, Debug)]
pub(super) enum Status {
    /// The request breaks a rule of the closure's, the model provider refused it or the route's
    /// key, or it comes once the route has ended.
    BadRequest,
    /// Its target is not `/responses`.
    NotFound,
    /// Its target is, but its method is not `POST`.
    MethodNotAllowed,
    /// The upstream gave no answer.
    BadGateway,
    /// The upstream gave no answer in time.
    GatewayTimeout,
}

impl Route {
    /// The route that allows each model that `models` keys, which goes upstream under the name
    /// that it maps the model to.
    pub fn new(models: BTreeMap<ModelName, ModelName>) -> Self {
        Self { models }
    }

    /// Parses `request` into one that keeps the route closed, naming its model as its upstream
    /// takes it.
    ///
    /// # Errors
    ///
    /// Refuses a request that breaks a rule that [the route's docs](crate) give.
    pub(crate) fn close(&self, request: &ModelRequest) -> Result<Closed, Refusal> {
        let ModelRequest {
            method,
            path,
            headers,
            body,
        } = request;
        if path != "/responses" {
            let message = format!("`{}` is not served: send POST /responses", shown(path));
            return Err(refusal(Status::NotFound, message));
        }
        if method != "POST" {
            let message = format!(
                "`{} /responses` is not served: send POST /responses",
                shown(method)
            );
            return Err(refusal(Status::MethodNotAllowed, message));
        }
        let headers = passed(headers)?;
        let mut body: Map<String, Value> = serde_json::from_slice(body).map_err(|error| {
            let error = error.to_string();
            bad(format!(
                "the body is not a JSON object ({}): send one",
                shown(&error)
            ))
        })?;
        stateless(&body)?;
        let (model, upstream_name) = self.allows(&body)?;
        picks_no_feature(&body)?;
        closed_fields(&body)?;
        let named = Value::String(upstream_name.as_str().to_owned());
        body.insert("model".to_owned(), named);
        // What goes upstream is the body as parsed here, not as sent, so that no other parser
        // can read it otherwise: one that keeps the first of a field sent twice, say, where this
        // keeps the last.
        let body = Value::Object(body).to_string().into_bytes();
        let model = (model.clone(), upstream_name.clone());
        Ok(Closed {
            headers,
            body,
            model,
        })
    }

    /// The model that `body` names, with the name that the upstream takes for it, refusing a
    /// request that names no model the route allows.
    fn allows(&self, body: &Map<String, Value>) -> Result<(&ModelName, &ModelName), Refusal> {
        let named = body.get("model").and_then(Value::as_str);
        let allowed =
            named.and_then(|named| (self.models.iter()).find(|(model, _)| model.as_str() == named));
        if let Some(allowed) = allowed {
            return Ok(allowed);
        }
        let allowed: Vec<String> = self
            .models
            .keys()
            .map(|model| format!("`{model}`"))
            .collect();
        let named = named.map_or("no model".to_owned(), |named| format!("`{}`", shown(named)));
        Err(bad(format!(
            "the request names {named}, which the route doesn't allow: name {}",
            allowed.join(" or ")
        )))
    }
}

impl Closed {
    /// The request's headers that pass upstream.
    pub(crate) fn headers(&self) -> &[(HeaderName, HeaderValue)] {
        &self.headers
    }

    /// The request's body, as [`Route::close`] parsed it.
    pub(crate) fn body(&self) -> &[u8] {
        &self.body
    }

    /// `answered`, the model that a response says answered the request, as the request named
    /// it, if it is the name that the upstream takes for it, and else as it is.
    pub(super) fn named<'a>(&'a self, answered: &'a str) -> &'a str {
        let (model, upstream_name) = &self.model;
        // A harness compares the model that answered with the one it asked for, ASCII case
        // aside, and would take the upstream's name for it as another model.
        if answered.eq_ignore_ascii_case(upstream_name.as_str()) {
            model.as_str()
        } else {
            answered
        }
    }
}

impl Refusal {
    /// The refusal of a request taken once the agent's work phase has ended, with which its
    /// route ends.
    pub(crate) fn ended() -> Self {
        // A 400, as the harness the runs use takes one as final, and ends its turn, so that it
        // can exit within the grace; a 403, say, it tries again, as it would a failed stream,
        // for as long as its retries last.
        let message = "the work phase has ended, and with it the model route: make no more model \
                       requests";
        refusal(Status::BadRequest, message.to_owned())
    }

    /// The refusal of a request once the model provider has refused the route's key with a 401,
    /// with which the route ends.
    pub(crate) fn key_refused() -> Self {
        // A 400, as for the work phase's end, so that the harness ends its turn rather than try
        // again with the same key, which every later request would carry.
        let message = "the model provider refused the route's key, with a 401, which ends the \
                       model route: make no more model requests";
        refusal(Status::BadRequest, message.to_owned())
    }

    /// The refusal of a request that the model provider refused with a 403.
    pub(crate) fn forbidden() -> Self {
        // A 400, so that the harness ends its turn rather than try the same request again.
        let message = "the model provider refused the request, with a 403: send only requests \
                       that it allows";
        refusal(Status::BadRequest, message.to_owned())
    }

    /// How [`Refusal::answer`] answers its client, in place of the upstream's response with the
    /// status `upstream_status`, if one came.
    pub(crate) fn answered(
        &self,
        upstream_status: Option<u16>,
        request_id: Option<String>,
    ) -> Answer {
        Answer::Refused {
            upstream_status,
            request_id,
            status: self.status.code(),
            message: self.message.clone(),
        }
    }

    /// How [`Upstream::serve`](super::Upstream::serve) answers its client with the refusal of a
    /// request that never reached the upstream.
    pub(crate) fn barred(&self) -> Answer {
        Answer::Barred {
            status: self.status.code(),
            message: self.message.clone(),
        }
    }

    /// Answers `request` with the refusal by `by` ([`IncomingModelRequest::respond`]): its
    /// status, with `allow: POST` for a 405, and its message in a JSON error,
    /// `{"error": {"message", "type", "param", "code"}}`, whose `type` is
    /// `invalid_request_error` for a 4xx and `server_error` for a 5xx, and whose `param` and
    /// `code` are `null`.
    ///
    /// # Errors
    ///
    /// Fails if `by` passes first, or if the session has ended.
    pub(crate) fn answer(self, request: IncomingModelRequest, by: Deadline) -> io::Result<()> {
        let Self { status, message } = self;
        let mut headers = vec![("content-type".to_owned(), "application/json".to_owned())];
        // A 4xx is a request for its client to mend, which a client takes as final rather than
        // trying again; a 5xx, the route's own failure, it may try again. HTTP requires a 405 to
        // name the methods its target allows.
        let kind = match status {
            Status::BadRequest | Status::NotFound => "invalid_request_error",
            Status::MethodNotAllowed => {
                headers.push(("allow".to_owned(), "POST".to_owned()));
                "invalid_request_error"
            }
            Status::BadGateway | Status::GatewayTimeout => "server_error",
        };
        // The model provider's API gives its own errors in this form, with these `type`s for a
        // request it won't serve as sent and for its own failure, so a client reports the
        // message.
        let error = json!({
            "message": message,
            "type": kind,
            "param": null,
            "code": null,
        });
        let body = json!({ "error": error }).to_string();
        let mut response = request.respond(status.code(), headers, by)?;
        response.send(body.as_bytes())?;
        response.end()
    }
}

impl Status {
    fn code(self) -> u16 {
        match self {
            Self::BadRequest => 400,
            Self::NotFound => 404,
            Self::MethodNotAllowed => 405,
            Self::BadGateway => 502,
            Self::GatewayTimeout => 504,
        }
    }
}

pub(super) fn refusal(status: Status, message: String) -> Refusal {
    Refusal { status, message }
}

/// A refusal of a request that breaks a rule of the closure's.
fn bad(message: impl Into<String>) -> Refusal {
    refusal(Status::BadRequest, message.into())
}

/// `text`, which the client chose, as a refusal shows it.
///
/// It is whole if it is at most 256 bytes long, and otherwise cut to its first 256 bytes, or
/// fewer on a character's boundary, and `…`.
// A refusal repeats what the client chose only to point at it, so it needs no more, and each
// refusal stays small however large the request.
fn shown(text: &str) -> Cow<'_, str> {
    if text.len() <= MAX_SHOWN_BYTES {
        Cow::Borrowed(text)
    } else {
        let cut = &text[..text.floor_char_boundary(MAX_SHOWN_BYTES)];
        Cow::Owned(format!("{cut}…"))
    }
}

/// The request headers that pass upstream, the ones [`PASSED_HEADERS`] lists, each refused
/// unless its value is a valid HTTP field value.
// HTTP's grammar lets a value hold no control character but tab. A line break in a value would
// end the header early, and could slip upstream a header that the list keeps behind.
fn passed(headers: &[(String, String)]) -> Result<Vec<(HeaderName, HeaderValue)>, Refusal> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let listed = PASSED_HEADERS.into_iter().find(|listed| listed == name)?;
            let value = HeaderValue::from_str(value).map_err(|_| {
                bad(format!(
                    "the header `{name}` holds a control character: send values without any but \
                     tab"
                ))
            });
            Some(value.map(|value| (HeaderName::from_static(listed), value)))
        })
        .collect()
}

/// Refuses a request that keeps state on the provider's side, or refers to state kept there.
fn stateless(body: &Map<String, Value>) -> Result<(), Refusal> {
    // A request that leaves `store` out is stored too.
    if body.get("store") != Some(&Value::Bool(false)) {
        return Err(bad(
            "`store` is not false: set it to false, since the route keeps nothing on the \
             provider's side",
        ));
    }
    let kept = STATE_FIELDS.into_iter().find(|field| {
        body.get(*field)
            .is_some_and(|value| !matches!(value, Value::Null | Value::Bool(false)))
    });
    if let Some(field) = kept {
        return Err(bad(format!(
            "`{field}` refers to state kept on the provider's side, which the route refuses: \
             send the whole context instead"
        )));
    }
    let Some(Value::Array(items)) = body.get("input") else {
        return Ok(());
    };
    // The provider takes an item that names no `type` as a reference too, if it has an `id`.
    let referenced = items.iter().any(|item| match item.get("type") {
        Some(Value::String(kind)) => kind == "item_reference",
        None
        | Some(
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_),
        ) => true,
    });
    if referenced {
        return Err(bad(
            "an `input` item is an `item_reference` or names no `type`, so it may refer to an \
             item stored on the provider's side: send each item itself, with its `type`",
        ));
    }
    Ok(())
}

/// Refuses a request that picks a feature of the provider's beyond its model and its own tools:
/// an access program, or a `tool_choice` that names a tool the provider runs.
fn picks_no_feature(body: &Map<String, Value>) -> Result<(), Refusal> {
    // An access program is one that the deployment's key may be authorized for, which the
    // workflow didn't choose as it chose the models.
    if body
        .get("access_programs")
        .is_some_and(|value| !value.is_null())
    {
        return Err(bad(
            "`access_programs` picks a program of the provider's, which the route refuses: leave \
             it out",
        ));
    }
    // A choice that is an object names a kind of tool. The allowed tools are a list of tools,
    // which [`closed_fields`] checks as it checks any other.
    let Some(Value::Object(choice)) = body.get("tool_choice") else {
        return Ok(());
    };
    let kind = choice
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if matches!(kind, "function" | "custom" | "allowed_tools") {
        return Ok(());
    }
    Err(bad(format!(
        "the tool choice `{}` names a tool that runs on the provider's side, which the route \
         refuses: choose `auto`, `none`, `required`, or a client-side tool (function, custom or \
         allowed_tools)",
        shown(kind)
    )))
}

/// Refuses a request that holds a field that would open the route anywhere in its body: a
/// list of tools with one the provider would run, a URL the provider would fetch, or a file,
/// a container or a response kept on the provider's side.
fn closed_fields(fields: &Map<String, Value>) -> Result<(), Refusal> {
    fields.iter().try_for_each(|(field, value)| {
        closed_field(field, value)?;
        closed_below(value)
    })
}

// The JSON parser bounds how deeply values nest, and so this recursion.
fn closed_below(value: &Value) -> Result<(), Refusal> {
    match value {
        Value::Object(fields) => closed_fields(fields),
        Value::Array(items) => items.iter().try_for_each(closed_below),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Ok(()),
    }
}

/// Refuses one field that would open the route: a `tools` list with a tool the provider would
/// run, a URL field that holds no inline data, or a `file_id`, `container_id` or
/// `comparison_response_id` that names a file, a container or a response. A field of another
/// type names nothing, such as a function's parameter that its schema calls `file_id`.
fn closed_field(field: &str, value: &Value) -> Result<(), Refusal> {
    match (field, value) {
        ("tools", Value::Array(tools)) => tools.iter().try_for_each(client_tool),
        ("file_id", Value::String(_)) => Err(bad(
            "`file_id` refers to a file stored on the provider's side, which the route refuses: \
             send the content inline",
        )),
        ("container_id", Value::String(_)) => Err(bad(
            "`container_id` refers to a container kept on the provider's side, which the route \
             refuses: run commands through a client-side tool",
        )),
        ("comparison_response_id", Value::String(_)) => Err(bad(
            "`comparison_response_id` refers to a response stored on the provider's side, which \
             the route refuses: leave it out",
        )),
        // Only a `data:` URL holds its content. Any other the provider may fetch, and URL
        // parsers forgive much: they drop spaces and control characters around a URL, and tabs
        // and line breaks within it, so no test of a scheme's prefix tells them all apart.
        (field, Value::String(url)) if URL_FIELDS.contains(&field) && !url.starts_with("data:") => {
            Err(bad(format!(
                "`{field}` holds a URL the provider would fetch, which the route refuses: send \
                 the content inline, as a `data:` URL"
            )))
        }
        _ => Ok(()),
    }
}

/// Refuses a tool the provider would run. A namespace's own tools are a list of tools of
/// their own, which [`closed_fields`] finds.
// A tool the provider runs works beyond the route: one searches the web, another runs code with
// its own network, another reads stored files. A client-side tool's calls come back to the
// client, which runs them in its sandbox.
fn client_tool(tool: &Value) -> Result<(), Refusal> {
    let kind = tool.get("type").and_then(Value::as_str).unwrap_or_default();
    let client = match kind {
        "function" | "custom" | "namespace" => true,
        "tool_search" => tool.get("execution").and_then(Value::as_str) == Some("client"),
        _ => false,
    };
    if client {
        return Ok(());
    }
    Err(bad(format!(
        "the tool `{}` runs on the provider's side, which the route refuses: declare \
         client-side tools only (function, custom, namespace, and tool_search with client \
         execution)",
        shown(kind)
    )))
}
