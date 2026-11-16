//! The rules of a route: which model requests it admits, and the form in which it sends them on.
//!
//! An entry admits a request only if it is a `POST` to its API's endpoint, passes on only the
//! headers of its API's own and those the entry lists, and holds a JSON object that names a model
//! the route allows. Nowhere in that object may it keep state on the provider's side, run a tool
//! there or have the provider fetch a URL that isn't inline data, nor pick a feature of the
//! provider's beyond the models and tools: `RESPONSES`, `CHAT` and `MESSAGES` name the fields.
//! Any other request is answered with a [`Refusal`] that says what to send instead.
//!
//! The rules refuse the forms they name and pass every other field. They read the body's JSON
//! fields, never the text of its context, and the body that goes upstream is the one they read,
//! its model named as the upstream takes it.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io;

use gateway::{IncomingModelRequest, ModelRequest};
use schema::ir::{Api, ModelBudget, ModelName};
use serde_json::{Map, Value, json};
use stdx::Deadline;
use ureq::http::{HeaderName, HeaderValue};

use super::upstream::Answer;
use super::{read, usage};
use crate::Apart;

// The request headers of each API's own: the form of the request and of its answer. A client's
// product adds its own, which its entry lists ([`Headers`]). Any other could reach more of the
// provider than the models the route allows, under the deployment's key: a credential of the
// agent's own, or a header that picks the account, the project, a model or a feature. An unlisted
// header stays behind without a refusal, so a list names each header whole, and passes none for
// sharing a prefix with one.
const RESPONSES_HEADERS: [&str; 2] = ["content-type", "accept"];

// `user-agent` is the one by which a client names itself that the provider takes; it needs none
// of the further, such as an SDK's `x-stainless-*`.
const CHAT_HEADERS: [&str; 3] = ["content-type", "accept", "user-agent"];

// `anthropic-version` is the version of the API that the client writes in.
const MESSAGES_HEADERS: [&str; 3] = ["content-type", "accept", "anthropic-version"];

// The response headers of each API's own: the form of the body, the provider's id for the
// request, and when to try again. Any other stays behind unless the entry lists it: one that
// names the account or the project that the deployment's key belongs to, which are not the
// agent's to know, and those that frame the body on the upstream's connection, since the session
// carries it in frames of its own.
const RESPONSE_HEADERS: [&str; 3] = ["content-type", "x-request-id", "retry-after"];

const MESSAGES_RESPONSE_HEADERS: [&str; 3] = ["content-type", "request-id", "retry-after"];

/// The fields of the Responses API that refer to state kept on the provider's side.
const STATE_FIELDS: [&str; 4] = [
    "previous_response_id",
    "conversation",
    "prompt",
    "background",
];

/// The fields that hold a URL, which the provider fetches unless it is a `data:` URL.
const URL_FIELDS: [&str; 4] = ["url", "image_url", "file_url", "audio_url"];

/// The most names of left-behind request headers that [`Keep::dropped_headers`] is given for a
/// request.
///
/// [`Keep::dropped_headers`]: crate::Keep::dropped_headers
// A client sends a few dozen headers at most; the names are for a person to read.
pub const MAX_DROPPED_HEADERS: usize = 32;

/// The most bytes of a value the client chose, such as a path or a model's name, that [`shown`]
/// keeps.
// A refusal repeats what the client chose only to point at it, so it needs no more, and each
// refusal stays small however large the request.
const MAX_SHOWN_BYTES: usize = 256;

/// What differs between the APIs: the endpoints, headers and checks of each.
#[derive(Debug)]
struct Rules {
    /// The targets of the API's endpoint: the first is its own, and any other is it with a query
    /// that goes upstream with it.
    targets: &'static [&'static str],
    headers: &'static [&'static str],
    response_headers: &'static [&'static str],
    apart: Apart,
    usage: usage::Reported,
    failure_code: &'static str,
    api: Api,
    stored: fn(&Map<String, Value>) -> Result<(), Refusal>,
    feature: fn(&Map<String, Value>) -> Result<(), Refusal>,
    tool: Tool,
    choice: fn(&Map<String, Value>) -> Result<(), Refusal>,
    /// Refuses an object, anywhere in the body, that has the provider fetch its content, and else
    /// gives its field, if any, that holds the client's own data, which no rule reads.
    object: Object,
}

type Tool = fn(&Value) -> Result<(), Refusal>;

type Object = fn(&Map<String, Value>) -> Result<Option<&'static str>, Refusal>;

const RESPONSES: Rules = Rules {
    targets: &["/responses"],
    headers: &RESPONSES_HEADERS,
    response_headers: &RESPONSE_HEADERS,
    apart: Apart {
        conversation: "input",
        whole: &["tools", "instructions"],
    },
    usage: usage::RESPONSES,
    failure_code: "code",
    api: Api::Responses,
    stored: stateless,
    feature: picks_no_feature,
    tool: client_tool,
    choice: chooses_client_tool,
    object: |_| Ok(None),
};

const CHAT: Rules = Rules {
    targets: &["/v1/chat/completions"],
    headers: &CHAT_HEADERS,
    response_headers: &RESPONSE_HEADERS,
    apart: Apart {
        conversation: "messages",
        whole: &["tools"],
    },
    usage: usage::CHAT,
    failure_code: "code",
    api: Api::Chat,
    stored: stores_nothing,
    feature: searches_nothing,
    tool: function_tool,
    choice: chooses_client_tool,
    object: |_| Ok(None),
};

const MESSAGES: Rules = Rules {
    // With the API's beta namespace, to which Anthropic's SDKs send a request through their beta
    // client: the features a beta turns on, the rules refuse by their fields.
    targets: &["/v1/messages", "/v1/messages?beta=true"],
    headers: &MESSAGES_HEADERS,
    response_headers: &MESSAGES_RESPONSE_HEADERS,
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

/// The entry of one API: it admits requests for the models that its workflow allows.
#[derive(Debug)]
pub struct Route {
    rules: &'static Rules,
    /// Each model that requests may name, with the name that its upstream takes for it.
    models: BTreeMap<ModelName, ModelName>,
    request_headers: Vec<HeaderName>,
    response_headers: Vec<String>,
}

/// The headers that a client's product adds to its API's own, which a route's entry passes as
/// sent, as the entry of the IR lists them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Headers {
    pub request: Vec<schema::ir::HeaderName>,
    pub response: Vec<schema::ir::HeaderName>,
}

/// A model request that keeps the route closed, ready to go upstream. Only [`Route::close`]
/// makes one.
#[derive(Debug)]
pub(crate) struct Closed<'r> {
    query: Option<&'static str>,
    headers: Vec<(HeaderName, HeaderValue)>,
    dropped_headers: Vec<String>,
    body: Vec<u8>,
    /// The model that the request named, and the name that its upstream takes for it, which
    /// `body` names.
    model: (ModelName, ModelName),
    route: &'r Route,
}

/// Why the route answers a model request with an error: a rule broken, the route's end, or an
/// upstream's answer missing or not passed on.
#[derive(Debug)]
pub(crate) struct Refusal {
    status: Status,
    message: String,
}

/// The status of a refusal: 404 for a target that is no entry's endpoint, 405 for a method that is
/// not `POST`, 502 and 504 for an upstream that gave no answer, or none in time, and 400 for any
/// other.
#[derive(Clone, Copy, Debug)]
pub(super) enum Status {
    BadRequest,
    NotFound,
    MethodNotAllowed,
    BadGateway,
    GatewayTimeout,
}

impl Route {
    /// The entry for `api` that allows each model that `models` keys, which goes upstream under
    /// the name that it maps the model to, and passes the headers of `api`'s own and `headers`.
    pub fn new(api: Api, models: BTreeMap<ModelName, ModelName>, headers: &Headers) -> Self {
        let rules = match api {
            Api::Responses => &RESPONSES,
            Api::Chat => &CHAT,
            Api::Messages => &MESSAGES,
        };
        let Headers { request, response } = headers;
        let joined = |own: &'static [&'static str], listed: &[schema::ir::HeaderName]| {
            let mut joined: Vec<String> = own.iter().map(|&name| name.to_owned()).collect();
            joined.extend(listed.iter().map(|name| name.as_str().to_owned()));
            joined.sort_unstable();
            joined.dedup();
            joined
        };
        let request_headers = joined(rules.headers, request).into_iter().map(|name| {
            HeaderName::try_from(name)
                .unwrap_or_else(|_| unreachable!("a lowercase token is an HTTP header's name"))
        });
        Self {
            rules,
            models,
            request_headers: request_headers.collect(),
            response_headers: joined(rules.response_headers, response),
        }
    }

    pub(crate) fn api(&self) -> Api {
        self.rules.api
    }

    /// Whether `name`, a response header in lowercase, reaches the client.
    pub(crate) fn returns(&self, name: &str) -> bool {
        self.response_headers.iter().any(|listed| listed == name)
    }

    /// Whether `request` is addressed to one of this route's endpoints, whatever else it holds.
    pub(crate) fn takes(&self, request: &ModelRequest) -> bool {
        self.target(&request.path).is_some()
    }

    fn target(&self, path: &str) -> Option<&'static str> {
        (self.rules.targets.iter())
            .find(|&&target| target == path)
            .copied()
    }

    /// The form of `request` that goes upstream, with its model named as its upstream takes it, or
    /// the refusal that answers it. `body` is the request's body as [`parsed`] gives it.
    pub(crate) fn close(
        &self,
        request: &ModelRequest,
        body: Result<Map<String, Value>, Refusal>,
    ) -> Result<Closed<'_>, Refusal> {
        let ModelRequest {
            method,
            path,
            headers,
            body: _,
        } = request;
        if method != "POST" {
            let message = format!("`{} {path}` is not served: send POST {path}", shown(method));
            return Err(refusal(Status::MethodNotAllowed, message));
        }
        let rules = self.rules;
        let query = (self.target(path))
            .and_then(|target| target.split_once('?'))
            .map(|(_, query)| query);
        let dropped_headers = self.dropped(headers);
        let headers = passed(&self.request_headers, headers)?;
        let mut body = body?;
        (rules.stored)(&body)?;
        let named = body.get("model").and_then(Value::as_str);
        let allowed = named.and_then(|named| self.allows(named));
        let (model, upstream_name) =
            allowed.ok_or_else(|| Refusal::unallowed(named, self.models.keys()))?;
        (rules.feature)(&body)?;
        (rules.choice)(&body)?;
        closed_fields(rules, &body)?;
        let named = Value::String(upstream_name.as_str().to_owned());
        body.insert("model".to_owned(), named);
        // What goes upstream is the body as parsed here, not as sent, so that no other parser can
        // read it otherwise: one that keeps the first of a field sent twice, say, where this
        // keeps the last.
        let body = Value::Object(body).to_string().into_bytes();
        let model = (model.clone(), upstream_name.clone());
        Ok(Closed {
            query,
            headers,
            dropped_headers,
            body,
            model,
            route: self,
        })
    }

    fn dropped(&self, headers: &[(String, String)]) -> Vec<String> {
        let mut dropped: Vec<String> = (headers.iter())
            .map(|(name, _)| name)
            .filter(|name| {
                !self
                    .request_headers
                    .iter()
                    .any(|listed| listed == name.as_str())
            })
            // One that no entry may list, such as `authorization`, the route always leaves behind.
            .filter(|name| schema::ir::HeaderName::try_from((*name).clone()).is_ok())
            .cloned()
            .collect();
        dropped.sort_unstable();
        dropped.dedup();
        dropped.truncate(MAX_DROPPED_HEADERS);
        dropped
    }

    /// The model that requests may name as `named`, with the name that its upstream takes for it,
    /// if they may.
    pub(crate) fn allows(&self, named: &str) -> Option<(&ModelName, &ModelName)> {
        (self.models.iter()).find(|(model, _)| model.as_str() == named)
    }

    /// Each model that requests may name.
    pub(crate) fn models(&self) -> impl Iterator<Item = &ModelName> {
        self.models.keys()
    }
}

/// `body`, a request's, as the JSON object it holds, or the refusal of a body that holds none.
pub(crate) fn parsed(body: &[u8]) -> Result<Map<String, Value>, Refusal> {
    serde_json::from_slice(body).map_err(|error| {
        let error = error.to_string();
        bad(format!(
            "the body is not a JSON object ({}): send one",
            shown(&error)
        ))
    })
}

impl Closed<'_> {
    pub(crate) fn route(&self) -> &Route {
        self.route
    }

    /// The names of the request's headers that the route leaves behind, those an entry could
    /// list: sorted, each once, and at most `MAX_DROPPED_HEADERS`.
    pub(crate) fn dropped_headers(&self) -> &[String] {
        &self.dropped_headers
    }

    /// The query that the upstream's URL takes, as the request's target carried it.
    pub(crate) fn query(&self) -> Option<&'static str> {
        self.query
    }

    pub(crate) fn headers(&self) -> &[(HeaderName, HeaderValue)] {
        &self.headers
    }

    pub(crate) fn model(&self) -> &ModelName {
        &self.model.0
    }

    /// The request's body, as [`Route::close`] parsed it.
    pub(crate) fn body(&self) -> &[u8] {
        &self.body
    }

    pub(crate) fn apart(&self) -> Apart {
        self.route.rules.apart
    }

    pub(crate) fn scan(&self, content_type: Option<&str>) -> usage::Scan {
        let rules = self.route.rules;
        let output = read::dialect(rules.api).output;
        usage::Scan::new(content_type, rules.usage, rules.failure_code, output)
    }

    /// `answered`, the model that a response says answered the request, as the request named it,
    /// if it is the name that the upstream takes for it, and else as it is.
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
    /// The refusal of a request to none of `routes`' endpoints, which names each.
    pub(crate) fn unserved<'a>(path: &str, routes: impl Iterator<Item = &'a Route>) -> Self {
        let mut targets: Vec<String> = Vec::new();
        for route in routes {
            // Several entries may be for one API.
            let target = format!("POST {}", route.rules.targets[0]);
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        let message = format!(
            "`{}` is not served: send {}",
            shown(path),
            targets.join(" or ")
        );
        refusal(Status::NotFound, message)
    }

    /// The refusal of a request that names `named`, or no model, which none of `allowed` is.
    pub(crate) fn unallowed<'a>(
        named: Option<&str>,
        allowed: impl Iterator<Item = &'a ModelName>,
    ) -> Self {
        let allowed: Vec<String> = allowed.map(|model| format!("`{model}`")).collect();
        let named = named.map_or("no model".to_owned(), |named| format!("`{}`", shown(named)));
        bad(format!(
            "the request names {named}, which the route doesn't allow: name {}",
            allowed.join(" or ")
        ))
    }

    pub(crate) fn ended() -> Self {
        // A 400, which a harness takes as final, where it would retry a 403. The message names
        // no cause, as the route knows no run.
        let message = "the model route has ended: make no more model requests";
        refusal(Status::BadRequest, message.to_owned())
    }

    /// The refusal of a request once the model provider has refused the route's key with a 401,
    /// which ends the route.
    pub(crate) fn key_refused() -> Self {
        // A 400, as for the route's end, so that the harness ends its turn rather than try
        // again with the same key, which every later request would carry.
        let message = "the model provider refused the route's key, with a 401, which ends the \
                       model route: make no more model requests";
        refusal(Status::BadRequest, message.to_owned())
    }

    /// The refusal of a request through `route`, whose `budget` is spent.
    pub(crate) fn spent(budget: &ModelBudget, route: &Route) -> Self {
        let models: Vec<String> = route.models().map(|model| format!("`{model}`")).collect();
        // A 400, as for the route's end, which a harness takes as final.
        let message = format!(
            "the model budget of the role's entry for {}, {budget} at the configured prices, is \
             exhausted: make no more requests for its models",
            models.join(", ")
        );
        refusal(Status::BadRequest, message)
    }

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

    /// The answer for a request that never reached the upstream.
    pub(crate) fn barred(&self) -> Answer {
        Answer::Barred {
            status: self.status.code(),
            message: self.message.clone(),
        }
    }

    /// Answers `request`, of `api`, with the refusal: its status, with `allow: POST` for a 405, and
    /// its [`body`](Refusal::body). Fails if `by` passes first.
    pub(crate) fn answer(
        self,
        api: Api,
        request: IncomingModelRequest,
        by: Deadline,
    ) -> io::Result<()> {
        let mut headers = vec![("content-type".to_owned(), "application/json".to_owned())];
        if let Status::MethodNotAllowed = self.status {
            // HTTP requires a 405 to name the methods its target allows.
            headers.push(("allow".to_owned(), "POST".to_owned()));
        }
        let body = self.body(api);
        let mut response = request.respond(self.status.code(), headers, by)?;
        response.send(body.as_bytes())?;
        response.end()
    }

    /// The body that answers its client, of `api`: its message in a JSON error of `api`'s form.
    pub(crate) fn body(&self, api: Api) -> String {
        if api == Api::Messages {
            // As the Messages API's own errors are, of the types its documentation gives for
            // each status: https://platform.claude.com/docs/en/api/errors
            let kind = match self.status {
                Status::BadRequest | Status::MethodNotAllowed => "invalid_request_error",
                Status::NotFound => "not_found_error",
                Status::BadGateway => "api_error",
                Status::GatewayTimeout => "timeout_error",
            };
            let error = json!({ "type": kind, "message": self.message });
            return json!({ "type": "error", "error": error }).to_string();
        }
        // A 4xx is a request for its client to mend, which a client takes as final rather than
        // trying again; a 5xx, the route's own failure, it may try again.
        let kind = match self.status {
            Status::BadRequest | Status::NotFound | Status::MethodNotAllowed => {
                "invalid_request_error"
            }
            Status::BadGateway | Status::GatewayTimeout => "server_error",
        };
        // The model provider's API gives its own errors in this form, with these `type`s for a
        // request it won't serve as sent and for its own failure, so a client reports the
        // message.
        let error = json!({
            "message": self.message,
            "type": kind,
            "param": null,
            "code": null,
        });
        json!({ "error": error }).to_string()
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

fn bad(message: impl Into<String>) -> Refusal {
    refusal(Status::BadRequest, message.into())
}

fn shown(text: &str) -> Cow<'_, str> {
    if text.len() <= MAX_SHOWN_BYTES {
        Cow::Borrowed(text)
    } else {
        let cut = &text[..text.floor_char_boundary(MAX_SHOWN_BYTES)];
        Cow::Owned(format!("{cut}…"))
    }
}

/// The request headers that pass upstream, the ones `listed`, each refused unless its value is a
/// valid HTTP field value.
// HTTP's grammar lets a value hold no control character but tab. A line break in a value would
// end the header early, and could slip upstream a header that the list keeps behind.
fn passed(
    listed: &[HeaderName],
    headers: &[(String, String)],
) -> Result<Vec<(HeaderName, HeaderValue)>, Refusal> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let listed = listed.iter().find(|listed| listed.as_str() == name)?;
            let value = HeaderValue::from_str(value).map_err(|_| {
                bad(format!(
                    "the header `{name}` holds a control character: send values without any but \
                     tab"
                ))
            });
            Some(value.map(|value| (listed.clone(), value)))
        })
        .collect()
}

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
        return Err(kept_state(field));
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

fn kept_state(field: &str) -> Refusal {
    bad(format!(
        "`{field}` refers to state kept on the provider's side, which the route refuses: send the \
         whole context instead"
    ))
}

fn stores_nothing(body: &Map<String, Value>) -> Result<(), Refusal> {
    // Left out, `store` is false.
    match body.get("store") {
        None | Some(Value::Null | Value::Bool(false)) => Ok(()),
        Some(_) => Err(bad(
            "`store` is not false: leave it out, or set it to false, since the route keeps \
             nothing on the provider's side",
        )),
    }
}

fn searches_nothing(body: &Map<String, Value>) -> Result<(), Refusal> {
    if body
        .get("web_search_options")
        .is_some_and(|value| !value.is_null())
    {
        return Err(bad(
            "`web_search_options` has the provider search the web, which the route refuses: \
             leave it out",
        ));
    }
    Ok(())
}

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
    Ok(())
}

fn chooses_client_tool(body: &Map<String, Value>) -> Result<(), Refusal> {
    // A choice that is an object names a kind of tool. The allowed tools are a list of tools,
    // which `closed_fields` checks as it checks any other.
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
/// list of tools with one that `rules` refuse, an object they refuse, a URL the provider would
/// fetch, or a file, a container or a response kept on the provider's side. A field that holds
/// the client's own data, as `rules` find it, is passed over.
fn closed_fields(rules: &Rules, fields: &Map<String, Value>) -> Result<(), Refusal> {
    let own = (rules.object)(fields)?;
    (fields.iter())
        .filter(|(field, _)| own != Some(field.as_str()))
        .try_for_each(|(field, value)| {
            closed_field(rules.tool, field, value)?;
            closed_below(rules, value)
        })
}

// The JSON parser bounds how deeply values nest, and so this recursion.
fn closed_below(rules: &Rules, value: &Value) -> Result<(), Refusal> {
    match value {
        Value::Object(fields) => closed_fields(rules, fields),
        Value::Array(items) => items.iter().try_for_each(|item| closed_below(rules, item)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Ok(()),
    }
}

/// Refuses one field that would open the route: a `tools` list with a tool that `tool` refuses,
/// a URL field that holds no inline data, or a `file_id`, `container_id` or
/// `comparison_response_id` that names a file, a container or a response. A field of another
/// type names nothing, such as a function's parameter that its schema calls `file_id`.
fn closed_field(tool: Tool, field: &str, value: &Value) -> Result<(), Refusal> {
    match (field, value) {
        ("tools", Value::Array(tools)) => tools.iter().try_for_each(tool),
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
/// their own, which `closed_fields` finds.
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

fn function_tool(tool: &Value) -> Result<(), Refusal> {
    let kind = tool.get("type").and_then(Value::as_str).unwrap_or_default();
    if matches!(kind, "function" | "custom") {
        return Ok(());
    }
    Err(bad(format!(
        "the tool `{}` runs on the provider's side, which the route refuses: declare client-side \
         tools only (function or custom)",
        shown(kind)
    )))
}

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
