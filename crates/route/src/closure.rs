//! Parsing a sandbox's model request into one that keeps the route closed.
//!
//! An entry of the Responses API admits a request that:
//!
//! - is `POST /responses`, the one endpoint through which a harness samples its model;
//! - passes on only the API's own headers (`RESPONSES_HEADERS`) and those its entry lists, each
//!   with a value that holds no control character but tab, and leaves every other behind;
//! - holds a JSON object that sets `store` to `false` and refers to no state kept on the
//!   provider's side, such as by `previous_response_id` or `file_id`;
//! - names a model the route allows, and no `access_programs` but `null`;
//! - declares and chooses client-side tools only, in every list of tools anywhere in the body;
//! - holds no URL but inline data (`URL_FIELDS`).
//!
//! The Chat Completions API's entry admits what `CHAT`'s doc says, and the Messages API's what
//! `MESSAGES`' doc says.
//!
//! The rules refuse the forms they name and pass every other field. They read the body's JSON
//! fields, never the text of its context, and the body that goes upstream is the one they read,
//! its model named as the upstream takes it. A refusal's status is 404 if the target is no
//! entry's endpoint, 405 if the method is not `POST`, and 400 for any other rule broken.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io;

use gateway::{IncomingModelRequest, ModelRequest};
use schema::ir::{Api, ModelName};
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

// Those by which a client names itself further, such as an SDK's `x-stainless-*`, the provider
// needs none of.
const CHAT_HEADERS: [&str; 3] = ["content-type", "accept", "user-agent"];

// The version of the API that the client writes in.
const MESSAGES_HEADERS: [&str; 3] = ["content-type", "accept", "anthropic-version"];

// The response headers of each API's own: the form of the body, the provider's id for the
// request, and when to try again. Any other stays behind unless the entry lists it: one that
// names the account or the project that the deployment's key belongs to, which are not the
// agent's to know, and those that frame the body on the upstream's connection, since the session
// carries it in frames of its own.
const RESPONSE_HEADERS: [&str; 3] = ["content-type", "x-request-id", "retry-after"];

const MESSAGES_RESPONSE_HEADERS: [&str; 3] = ["content-type", "request-id", "retry-after"];

/// The fields that refer to state kept on the provider's side.
const STATE_FIELDS: [&str; 4] = [
    "previous_response_id",
    "conversation",
    "prompt",
    "background",
];

/// The fields whose URL the provider would fetch.
const URL_FIELDS: [&str; 4] = ["url", "image_url", "file_url", "audio_url"];

/// The most names of request headers left behind that [`Keep::dropped_headers`](crate::Keep) is
/// given for a request, and that a run's readout holds for all of them.
// A client sends a few dozen headers at most; the names are for a person to read.
pub const MAX_DROPPED_HEADERS: usize = 32;

/// The most bytes of a value the client chose, such as a path or a model's name, that
/// [`shown`] keeps.
const MAX_SHOWN_BYTES: usize = 256;

/// What an entry of one API admits, besides what every entry's rules keep alike.
#[derive(Debug)]
struct Rules {
    /// The targets of the API's endpoint: the first is its own, and any other is it with a query
    /// that goes upstream with it.
    targets: &'static [&'static str],
    /// The request headers of the API's own.
    headers: &'static [&'static str],
    /// The response headers of the API's own.
    response_headers: &'static [&'static str],
    /// The fields of a request's body that a record may keep apart.
    apart: Apart,
    /// How a response reports its usage.
    usage: usage::Reported,
    /// The field of a failure's error that gives its code.
    failure_code: &'static str,
    /// The API, whose reader says which events of a response's stream are the model's output.
    api: Api,
    /// Refuses a body that keeps state on the provider's side, or refers to state kept there.
    stored: fn(&Map<String, Value>) -> Result<(), Refusal>,
    /// Refuses a body that picks a feature of the provider's beyond its models and tools.
    feature: fn(&Map<String, Value>) -> Result<(), Refusal>,
    /// Refuses a tool, in any list of tools in the body, that the provider would run.
    tool: Tool,
    /// Refuses a tool choice that would have the provider run a tool.
    choice: fn(&Map<String, Value>) -> Result<(), Refusal>,
    /// Refuses an object, anywhere in the body, that has the provider fetch its content, and else
    /// gives its field, if any, that holds the client's own data, which no rule reads.
    object: Object,
}

/// A check of one tool that a list of tools declares.
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

/// An entry of the Chat Completions API admits a request that is `POST /v1/chat/completions`,
/// passes on only `CHAT_HEADERS` and those its entry lists, stores nothing on the provider's side
/// (no `store` but `false` or `null`), names a model the route allows and no
/// `web_search_options` but `null`, declares and chooses only `function` and `custom` tools, and
/// holds no URL but inline data and no `file_id`, `container_id` or `comparison_response_id`
/// anywhere.
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

/// An entry of the Messages API admits a request that is `POST /v1/messages`, or
/// `POST /v1/messages?beta=true`, passes on only `MESSAGES_HEADERS` and those its entry lists,
/// sets no `container`, no `thread`, no `mcp_servers` and no `fallbacks` but `null`, names a
/// model the route allows, declares only custom tools (no `type`, or `custom`) and chooses among
/// them, holds no content block whose `source` is a URL or a file, and no `container_upload`, and
/// holds no URL but inline data and no `file_id` anywhere but in a tool call's `input`.
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

/// A model route's entry for one API.
#[derive(Debug)]
pub struct Route {
    rules: &'static Rules,
    /// Each model that requests may name, with the name that its upstream takes for it.
    models: BTreeMap<ModelName, ModelName>,
    /// The request headers that pass upstream: the API's own and the entry's.
    request_headers: Vec<HeaderName>,
    /// The response headers that reach the client, in lowercase: the API's own and the entry's.
    response_headers: Vec<String>,
}

/// The headers that a client's product adds to its API's own, which a route's entry passes as
/// sent, as the entry of the IR lists them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Headers {
    /// The request headers that go upstream.
    pub request: Vec<schema::ir::HeaderName>,
    /// The response headers that reach the client.
    pub response: Vec<schema::ir::HeaderName>,
}

/// A model request that keeps the route closed, ready to go upstream. Only [`Route::close`]
/// makes one.
#[derive(Debug)]
pub(crate) struct Closed<'r> {
    /// The query that the request's target carried, which goes upstream with it.
    query: Option<&'static str>,
    headers: Vec<(HeaderName, HeaderValue)>,
    /// The names of the headers left behind that an entry could list.
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

#[derive(Clone, Copy, Debug)]
pub(super) enum Status {
    /// The request breaks a rule of the closure's, the model provider refused it or the route's
    /// key, or the route has ended before it was answered.
    BadRequest,
    /// Its target is no entry's endpoint.
    NotFound,
    /// Its target is, but its method is not `POST`.
    MethodNotAllowed,
    /// The upstream gave no answer.
    BadGateway,
    /// The upstream gave no answer in time.
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

    /// The API whose requests it takes.
    pub(crate) fn api(&self) -> Api {
        self.rules.api
    }

    /// Whether `name`, a response header in lowercase, reaches the client.
    pub(crate) fn returns(&self, name: &str) -> bool {
        self.response_headers.iter().any(|listed| listed == name)
    }

    /// Whether `request` is to its API's endpoint, which [`Route::close`] then takes.
    pub(crate) fn takes(&self, request: &ModelRequest) -> bool {
        self.target(&request.path).is_some()
    }

    /// The target of its API's endpoint that `path` is, if it is one.
    fn target(&self, path: &str) -> Option<&'static str> {
        (self.rules.targets.iter())
            .find(|&&target| target == path)
            .copied()
    }

    /// Parses `request`, which [`Route::takes`], into one that keeps the route closed, naming its
    /// model as its upstream takes it. Refuses a request that breaks a rule of
    /// [the route's docs](crate).
    pub(crate) fn close(&self, request: &ModelRequest) -> Result<Closed<'_>, Refusal> {
        let ModelRequest {
            method,
            path,
            headers,
            body,
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
        let mut body: Map<String, Value> = serde_json::from_slice(body).map_err(|error| {
            let error = error.to_string();
            bad(format!(
                "the body is not a JSON object ({}): send one",
                shown(&error)
            ))
        })?;
        (rules.stored)(&body)?;
        let (model, upstream_name) = self.allows(&body)?;
        (rules.feature)(&body)?;
        (rules.choice)(&body)?;
        closed_fields(rules, &body)?;
        let named = Value::String(upstream_name.as_str().to_owned());
        body.insert("model".to_owned(), named);
        // What goes upstream is the body as parsed here, not as sent, so that no other parser
        // can read it otherwise: one that keeps the first of a field sent twice, say, where this
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

    /// The names of `headers` that the entry leaves behind, as [`Closed::dropped_headers`] has
    /// them.
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

impl Closed<'_> {
    /// The route's entry that closed it.
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

    /// The model that the request named, as the route allows it.
    pub(crate) fn model(&self) -> &ModelName {
        &self.model.0
    }

    /// The request's body, as [`Route::close`] parsed it.
    pub(crate) fn body(&self) -> &[u8] {
        &self.body
    }

    /// The fields of its body that its API's client sends again on each request.
    pub(crate) fn apart(&self) -> Apart {
        self.route.rules.apart
    }

    /// A scan of its response's body, whose `content-type` is `content_type`, as its API reads
    /// one.
    pub(crate) fn scan(&self, content_type: Option<&str>) -> usage::Scan {
        let rules = self.route.rules;
        let output = read::dialect(rules.api).output;
        usage::Scan::new(content_type, rules.usage, rules.failure_code, output)
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
    /// The refusal of a request to none of `routes`' endpoints, which names each.
    pub(crate) fn unserved<'a>(path: &str, routes: impl Iterator<Item = &'a Route>) -> Self {
        let targets: Vec<String> = routes
            .map(|route| format!("POST {}", route.rules.targets[0]))
            .collect();
        let message = format!(
            "`{}` is not served: send {}",
            shown(path),
            targets.join(" or ")
        );
        refusal(Status::NotFound, message)
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

    /// Answers `request`, of `api`, with the refusal: its status, with `allow: POST` for a 405,
    /// and its [`body`](Refusal::body). Fails if `by` passes first.
    pub(crate) fn answer(
        self,
        api: Api,
        request: IncomingModelRequest,
        by: Deadline,
    ) -> io::Result<()> {
        let mut headers = vec![("content-type".to_owned(), "application/json".to_owned())];
        // HTTP requires a 405 to name the methods its target allows.
        if let Status::MethodNotAllowed = self.status {
            headers.push(("allow".to_owned(), "POST".to_owned()));
        }
        let body = self.body(api);
        let mut response = request.respond(self.status.code(), headers, by)?;
        response.send(body.as_bytes())?;
        response.end()
    }

    /// The body that answers its client, of `api`: its message in a JSON error of `api`'s form.
    ///
    /// Of an API of OpenAI's, it is `{"error": {"message", "type", "param", "code"}}`, whose
    /// `type` is `invalid_request_error` for a 4xx and `server_error` for a 5xx, and whose `param`
    /// and `code` are `null`. Of the Messages API, it is `{"type": "error", "error": {"type",
    /// "message"}}`, whose `type` is `not_found_error` for a 404, `invalid_request_error` for
    /// another 4xx, `api_error` for a 502 and `timeout_error` for a 504.
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
    // A conversation the provider holds under a response's `id`, which `create` starts and
    // `continue` resumes, so that a request sends only its new turn.
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
