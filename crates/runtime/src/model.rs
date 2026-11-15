//! The model routes that a run serves, by the rules of [`route`].

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use decision::{Outcome, Verdict, WorkEnd};
use gateway::IncomingModelRequest;
use route::{Answer, Apart, Headers, Keep, Route, Serving, Upstream};
use schema::ir::{Api, ApiRoute, Name, Sha256Digest};
use schema::workflow::{ByRole, Sandbox, Workflow};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};
use stdx::{Context as _, Deadline, unpoisoned};

use crate::{Clock, Stopped};

/// A sandbox's model route, as the run serves it: for each entry, the route that closes its
/// requests and the upstream that they reach, with the name that the deployment gives it and the
/// entry's API.
pub(super) struct Served {
    entries: Vec<(Route, Upstream)>,
    names: Vec<(Name, Api)>,
    /// How the model provider is failing the sandbox: the failure, if the last request answered
    /// while the route was open got one, and no 2xx came after it.
    failing: Mutex<Option<Failing>>,
}

/// The model provider's failure of a sandbox's requests, as the last one answered shows.
#[derive(Debug)]
struct Failing {
    /// The number of the first request of the failure.
    since: u64,
    /// The number of the last request answered, the upstream that answered it, and how it
    /// failed.
    last: (u64, Name, String),
}

impl fmt::Debug for Served {
    // The upstreams show nothing, as they hold the keys.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (f.debug_struct("Served").field("names", &self.names)).finish_non_exhaustive()
    }
}

/// Each role's model route in `workflow`, as `servings` serve it: for each entry of the route,
/// in its order, the serving of the entry's API, which takes the models that the entry allows
/// alone; none for a role without a route. Fails if an entry has no serving of its API, or
/// allows a model that its serving doesn't serve.
pub(super) fn routes(
    workflow: &Workflow,
    servings: &ByRole<Vec<Serving>>,
) -> Result<ByRole<Vec<Serving>>, String> {
    workflow.by_role(|(role, sandbox)| route(sandbox, servings.get(role)))
}

fn route(sandbox: &Sandbox, servings: &[Serving]) -> Result<Vec<Serving>, String> {
    let Some(route) = &sandbox.model_route else {
        return Ok(Vec::new());
    };
    let role = &sandbox.name;
    let served = |entry: &ApiRoute| {
        let ApiRoute {
            api,
            models,
            request_headers,
            response_headers,
        } = entry;
        let Some(serving) = servings.iter().find(|serving| serving.api == *api) else {
            return Err(format!(
                "`{role}` has a model route, but no upstream serves its `{api}` entry: give one, \
                 or remove the role's `egress.model` to run without one"
            ));
        };
        let taken = models.as_slice().iter().map(|model| {
            let Some(upstream_name) = serving.models.get(model) else {
                return Err(format!(
                    "`{role}`'s model route allows `{model}`, which its upstream doesn't serve: \
                     give an upstream that serves every model the route allows"
                ));
            };
            Ok((model.clone(), upstream_name.clone()))
        });
        Ok(Serving {
            name: serving.name.clone(),
            api: *api,
            url: serving.url.clone(),
            key: serving.key.clone(),
            models: taken.collect::<Result<_, String>>()?,
            headers: Headers {
                request: request_headers.clone(),
                response: response_headers.clone(),
            },
        })
    };
    route.as_slice().iter().map(served).collect()
}

impl Served {
    /// The route that `servings` serve, a role's as [`routes`] gives it, with upstreams of its
    /// own, or `None` for a role without one.
    pub(super) fn new(servings: &[Serving]) -> Option<Self> {
        let entries = servings.iter().map(|serving| {
            let route = Route::new(serving.api, serving.models.clone(), &serving.headers);
            (route, Upstream::new(&serving.url, &serving.key))
        });
        let names = (servings.iter()).map(|serving| (serving.name.clone(), serving.api));
        (!servings.is_empty()).then(|| Self {
            entries: entries.collect(),
            names: names.collect(),
            failing: Mutex::new(None),
        })
    }

    /// Answers `incoming`, the sandbox's model request numbered `number`, as [`route::serve`] does,
    /// keeping it in `exchange`, calling `forwarding` just before it sends it upstream, and
    /// telling `answered` how it answered it, and the upstream and API of the entry that took it,
    /// if one did, before the answer's end is written.
    pub(super) fn serve(
        &self,
        (number, exchange): (u64, &Exchange<impl Clock>),
        incoming: IncomingModelRequest,
        allowed: bool,
        by: Deadline,
        forwarding: impl FnOnce(),
        answered: impl FnOnce(Option<&(Name, Api)>, Answer),
    ) {
        let answered = |entry: Option<usize>, answer| {
            let taken = entry.map(|entry| &self.names[entry]);
            if let Some((name, _)) = taken {
                self.bear(number, name, &answer);
            }
            answered(taken, answer);
        };
        let entries = &self.entries;
        route::serve(
            entries, incoming, allowed, by, exchange, forwarding, answered,
        );
    }

    /// Notes how upstream `name` answered request `number`, if the route is open, in what the
    /// provider is failing the sandbox in.
    fn bear(&self, number: u64, name: &Name, answer: &Answer) {
        if self.ended() || matches!(answer, Answer::Barred { .. }) {
            return;
        }
        let mut failing = unpoisoned(self.failing.lock());
        *failing = match failure(answer) {
            Some(how) => {
                let since = failing.as_ref().map_or(number, |failing| failing.since);
                Some(Failing {
                    since,
                    last: (number, name.clone(), how),
                })
            }
            None => None,
        };
    }

    pub(super) fn key_refused_by(&self) -> Option<&Name> {
        let refused = self
            .entries
            .iter()
            .position(|(_, upstream)| upstream.key_was_refused());
        refused.map(|entry| &self.names[entry].0)
    }

    /// Checks that the sandbox's last request answered while the route was open got no failure
    /// from the model provider, if the agent's exit ended the work phase of a run that ended with
    /// `outcome` with no submission finalized. Fails, naming the upstream and the failure, if it
    /// did.
    pub(super) fn failed_last(&self, outcome: &Result<Outcome, Stopped>) -> Result<(), String> {
        // An agent that ran to the deadline, or finalized, is scored as usual: Harbor scores an
        // agent that timed out in its provider's backoff, and Inspect's `time_limit` is wall
        // clock, while none of the harnesses studied voids such a run.
        let failing = unpoisoned(self.failing.lock());
        let failed = failing.as_ref().filter(|_| exited_unfinalized(outcome));
        let Some(Failing { since, last }) = failed else {
            return Ok(());
        };
        let (number, name, how) = last;
        Err(format!(
            "the upstream `{name}` failed the agent's last model request, {number} ({how}; \
             failing since request {since}), and the agent exited: a deployment fault, not the \
             agent's result; run it again"
        ))
    }

    /// Ends the route's every exchange in flight and every later one ([`Upstream::shutdown`]),
    /// unless it has ended.
    pub(super) fn end(&self) {
        if self.ended() {
            return;
        }
        for (_, upstream) in &self.entries {
            upstream.shutdown();
        }
    }

    fn ended(&self) -> bool {
        self.entries
            .iter()
            .any(|(_, upstream)| upstream.was_shut_down())
    }
}

/// Whether `outcome` is one where the agent's exit ended the work phase, with no submission
/// finalized.
fn exited_unfinalized(outcome: &Result<Outcome, Stopped>) -> bool {
    let Ok(Outcome { work_end, verdict }) = outcome else {
        return false;
    };
    let finalized = match verdict {
        Verdict::NoSubmission => false,
        Verdict::Judged { submission, .. } | Verdict::NoResult { submission, .. } => {
            submission.finalized
        }
    };
    match work_end {
        WorkEnd::AgentExited(_) => !finalized,
        WorkEnd::Deadline | WorkEnd::Finalized | WorkEnd::AgentFailed(_) => false,
    }
}

/// How the provider failed the request that `answer` answers, or `None` if it didn't: a
/// provider's answer that the request caused, or a 2xx, clears a failure.
fn failure(answer: &Answer) -> Option<String> {
    // Inspect, Harbor and the OpenHands benchmarks keep out of their scores an agent whose model
    // calls failed past their retries. The route fixes the path and the deployment the model, so
    // a redirect, a 402 or a 404 (`model_not_found`) is the deployment's fault, as Harbor and
    // LiteLLM treat a 404; the other 4xx are the request's.
    match answer {
        Answer::Forwarded { status, .. } => match status {
            0..=299 | 400 | 405..=407 | 409..=428 | 430..=499 => None,
            300..=399 | 401..=404 | 408 | 429 | 500..=u16::MAX => Some(status.to_string()),
        },
        Answer::Cut { status, .. } => Some(format!("{status}, cut short")),
        Answer::Barred { .. } => None,
        Answer::Refused {
            upstream_status: Some(status @ (401 | 403)),
            ..
        } => Some(status.to_string()),
        Answer::Refused {
            upstream_status: Some(status),
            ..
        } => Some(format!("{status}, coded")),
        Answer::Refused {
            upstream_status: None,
            status,
            ..
        } => Some(format!("{status}, no answer")),
    }
}

/// A model exchange, as the record keeps it while it passes ([`Keep`]), in `model/` of the
/// record, numbered as the run log numbers requests: its request as `<n>.request` and its
/// answer's body as `<n>.response`, each made as its first bytes come. A request whose body the
/// route read keeps apart what its API lets it ([`Apart`]): each item of its conversation, and
/// each field kept whole that the body holds, such as its tools, once in `items/<hex>`, the JSON
/// named by its SHA-256; its body holds the digest of each in its place, and
/// [`Record::model_request`](super::Record::model_request) rebuilds it. It notes, by the run's
/// clock `C`, when its answer's first bytes came, and when its stream's output began.
pub(super) struct Exchange<C> {
    clock: C,
    /// When its answer's first bytes came, once they have, and its stream's output began, once
    /// it has.
    first: Cell<Option<(Duration, Option<Duration>)>>,
    dir: PathBuf,
    number: u64,
    request: Part,
    response: Part,
    /// The request's body, once it has come.
    body: RefCell<Option<Body>>,
    /// The names of the request's headers that the route left behind.
    dropped_headers: RefCell<Vec<String>>,
    /// The items it made, to sync at its end.
    items: RefCell<Vec<File>>,
    /// Why keeping it failed, if it did: it is then kept no further.
    failed: RefCell<Option<String>>,
}

/// What an exchange keeps of its request's body: its size and SHA-256, the field whose items are
/// kept apart, with their count, if they are, and the fields kept whole.
struct Body {
    bytes: u64,
    sha256: Sha256Digest,
    items: Option<(&'static str, usize)>,
    whole: Vec<&'static str>,
}

impl Body {
    /// What is kept of `body`, which went upstream, with `items` and `whole` kept apart.
    fn of(body: &[u8], items: Option<(&'static str, usize)>, whole: Vec<&'static str>) -> Self {
        Self {
            bytes: body.len() as u64,
            sha256: sha256(body),
            items,
            whole,
        }
    }
}

fn sha256(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest::from(<[u8; 32]>::from(Sha256::digest(bytes)))
}

struct Part {
    name: &'static str,
    file: RefCell<Option<File>>,
    bytes: Cell<u64>,
}

impl<C: Clock> Exchange<C> {
    /// Exchange `number`, to keep in the record at `record`, timed by `clock`.
    pub(super) fn new(record: &Path, number: u64, clock: C) -> Self {
        let part = |name| Part {
            name,
            file: RefCell::new(None),
            bytes: Cell::new(0),
        };
        Self {
            clock,
            first: Cell::new(None),
            dir: record.join("model"),
            number,
            request: part("request"),
            response: part("response"),
            body: RefCell::new(None),
            dropped_headers: RefCell::new(Vec::new()),
            items: RefCell::new(Vec::new()),
            failed: RefCell::new(None),
        }
    }

    /// What the run log's `model_answered` says of where it is kept so far: `{"request": {"path":
    /// p, "bytes": b, "body_bytes": s, "body_sha256": h, "items": {"field": f, "count": c},
    /// "whole": [w], "dropped_headers": [d]}, "response": {"path": p, "bytes": b}}`, each part once
    /// it has bytes, with the paths below the record: `s` the size of the request's body and `h`
    /// its SHA-256's 64 hexadecimal digits, `items` the field whose `c` items are kept apart, if
    /// any, `whole` the fields kept whole, in their API's order, if any, and `dropped_headers` the
    /// names of the request's headers that the route left behind ([`Keep::dropped_headers`]), if
    /// any.
    pub(super) fn kept(&self) -> Value {
        let mut kept = json!({});
        for part in [&self.request, &self.response] {
            if part.file.borrow().is_some() {
                let path = format!("model/{}.{}", self.number, part.name);
                kept[part.name] = json!({ "path": path, "bytes": part.bytes.get() });
            }
        }
        if let (Some(body), Some(request)) = (&*self.body.borrow(), kept.get_mut("request")) {
            let Body {
                bytes,
                sha256,
                items,
                whole,
            } = body;
            request["body_bytes"] = json!(bytes);
            request["body_sha256"] = json!(sha256.hex());
            if let Some((field, count)) = items {
                request["items"] = json!({ "field": field, "count": count });
            }
            if !whole.is_empty() {
                request["whole"] = json!(whole);
            }
            let dropped = self.dropped_headers.borrow();
            if !dropped.is_empty() {
                request["dropped_headers"] = json!(*dropped);
            }
        }
        kept
    }

    /// When its answer's first bytes came, if they have, and its stream's output began, if it has.
    pub(super) fn timed(&self) -> Option<(Duration, Option<Duration>)> {
        self.first.get()
    }

    /// Syncs what it kept to the record's disk, and returns why keeping it failed, if it did.
    pub(super) fn finish(self) -> Option<String> {
        let parts = [self.request, self.response].into_iter();
        let files = parts.filter_map(|part| part.file.into_inner());
        let synced = (files.chain(self.items.into_inner())).try_for_each(|file| file.sync_data());
        let failed = self.failed.into_inner();
        failed.or_else(|| {
            synced
                .err()
                .map(|error| format!("syncing it failed: {error}"))
        })
    }

    fn add(&self, part: &Part, bytes: &[u8]) {
        if bytes.is_empty() || self.failed.borrow().is_some() {
            return;
        }
        let mut file = part.file.borrow_mut();
        let written = match &mut *file {
            Some(file) => file.write_all(bytes),
            None => fs::create_dir_all(&self.dir)
                .and_then(|()| {
                    File::create_new(self.dir.join(format!("{}.{}", self.number, part.name)))
                })
                .and_then(|made| file.insert(made).write_all(bytes)),
        };
        match written {
            Ok(()) => part.bytes.set(part.bytes.get() + bytes.len() as u64),
            Err(error) => *self.failed.borrow_mut() = Some(format!("{} {error}", part.name)),
        }
    }

    /// `body` with the fields of `apart` kept apart, each once in the record ([`Exchange`]): each
    /// item of its conversation, if that is an array, and each field kept whole that it holds,
    /// with the digest of each in its place; and what it keeps of the body. `None` if `body` is no
    /// JSON object, or a part couldn't be kept.
    fn kept_apart(&self, body: &[u8], apart: Apart) -> Option<(Vec<u8>, Body)> {
        let Ok(Value::Object(mut fields)) = serde_json::from_slice(body) else {
            return None;
        };
        let dir = self.dir.join("items");
        fs::create_dir_all(&dir).ok()?;
        let items = match fields.get_mut(apart.conversation) {
            Some(Value::Array(items)) => {
                for item in items.iter_mut() {
                    *item = self.kept_item(&dir, item)?;
                }
                Some((apart.conversation, items.len()))
            }
            _ => None,
        };
        let mut whole = Vec::new();
        for &field in apart.whole {
            if let Some(value) = fields.get_mut(field) {
                *value = self.kept_item(&dir, value)?;
                whole.push(field);
            }
        }
        let rest = Value::Object(fields).to_string().into_bytes();
        Some((rest, Body::of(body, items, whole)))
    }

    /// Keeps `value` in `dir` as an item, once, named by the SHA-256 of its JSON, and returns
    /// its digest, as a JSON string; `None` if it couldn't be kept.
    fn kept_item(&self, dir: &Path, value: &Value) -> Option<Value> {
        // The body went upstream as `serde_json` writes it, so each item is written alike, and the
        // body rebuilt from the items' text is the body that went.
        let text = value.to_string();
        let digest = sha256(text.as_bytes());
        let path = dir.join(digest.hex());
        // Kept once: by an earlier request, or by another caller's at the same time. It is written
        // under this exchange's own name, then linked to its digest's, which no later link
        // replaces, so no other request sees its name before all of it is written, as git moves
        // an object into place:
        // https://github.com/git/git/blob/fb255012acbcbe81189cb8014da0280b09c66ecc/object-file.c#L382-L457
        if !path.try_exists().ok()? {
            let partial = dir.join(format!(".{}.partial", self.number));
            let mut file = File::create_new(&partial).ok()?;
            let linked =
                (file.write_all(text.as_bytes())).and_then(|()| fs::hard_link(&partial, &path));
            let removed = fs::remove_file(&partial);
            match linked {
                Ok(()) => self.items.borrow_mut().push(file),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(_) => return None,
            }
            removed.ok()?;
        }
        Some(Value::String(digest.as_str().to_owned()))
    }
}

/// Model request `number`'s body, rebuilt from the record at `record`, as
/// [`Record::model_request`](super::Record::model_request) says.
pub(super) fn rebuilt_request(record: &Path, number: u64) -> Result<Vec<u8>, String> {
    let log = record.join("log.jsonl");
    let text = fs::read_to_string(&log).context(log.display())?;
    let kept = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .map(|line| line["event"].clone())
        .find(|event| event["kind"] == "model_answered" && event["request"] == number)
        .map(|event| event["kept"]["request"].clone());
    let Some(Value::Object(kept)) = kept else {
        return Err(format!("the run log names no kept model request {number}"));
    };
    let body = rebuilt_body(record, &kept)?;
    if let Some(sent) = kept.get("body_sha256") {
        let rebuilt = sha256(&body);
        if sent.as_str() != Some(rebuilt.hex()) {
            let sent = sent
                .as_str()
                .map_or_else(|| sent.to_string(), str::to_owned);
            return Err(format!(
                "model request {number}: the record's items don't give back the body it sent: \
                 the rebuilt body's SHA-256 is {}, where the run log names {sent}",
                rebuilt.hex()
            ));
        }
    }
    Ok(body)
}

/// The body of the request that `kept`, its run log's `kept.request`, names in the record at
/// `record`.
fn rebuilt_body(record: &Path, kept: &Map<String, Value>) -> Result<Vec<u8>, String> {
    let Some(parts) = kept_parts(record, kept)? else {
        let path = kept_path(record, kept)?;
        return fs::read(&path).context(path.display());
    };
    let resolved = |part: KeptPart| match part {
        KeptPart::Sent(value) => Ok(value),
        KeptPart::Kept(digest) => {
            let text = kept_item(record, &digest)?;
            let path = record.join("model/items").join(digest.hex());
            serde_json::from_slice(&text).map_err(|error| format!("{}: {error}", path.display()))
        }
    };
    let KeptRequest {
        conversation,
        fields,
    } = parts;
    let mut body = Map::new();
    if let Some((field, items)) = conversation {
        let items = items.into_iter().map(resolved).collect::<Result<_, _>>()?;
        body.insert(field, Value::Array(items));
    }
    for (field, part) in fields {
        body.insert(field, resolved(part)?);
    }
    Ok(Value::Object(body).to_string().into_bytes())
}

/// A model request's body as the record keeps it
/// ([`Record::kept_request`](super::Record::kept_request)): each of its fields as it was sent, or
/// named by its digest where the record kept it apart.
#[derive(Debug, Clone, PartialEq)]
pub struct KeptRequest {
    /// The field that holds the conversation, and each of its items, the earliest first, if the
    /// record kept them apart.
    pub conversation: Option<(String, Vec<KeptPart>)>,
    /// The body's other fields.
    pub fields: Vec<(String, KeptPart)>,
}

/// A part of a kept request's body.
#[derive(Debug, Clone, PartialEq)]
pub enum KeptPart {
    /// Kept apart, once, in `model/items/` ([`Record::kept_item`](super::Record::kept_item)).
    Kept(Sha256Digest),
    /// Held in the request's own file, as the client sent it.
    Sent(Value),
}

/// The path of the file that `kept`, its run log's `kept.request` or `kept.response`, names in
/// the record at `record`, or the refusal of a path that may leave the record: one that is
/// absolute, that holds `..`, or that is a symbolic link, is the record's damage.
pub(super) fn kept_path(record: &Path, kept: &Map<String, Value>) -> Result<PathBuf, String> {
    let path = Path::new(kept.get("path").and_then(Value::as_str).unwrap_or_default());
    let below = (path.components()).all(|part| matches!(part, Component::Normal(_)));
    if !below || path.as_os_str().is_empty() {
        return Err(format!(
            "the run log names the kept file `{}`: expected a path below the record",
            path.display()
        ));
    }
    linked(record.join(path))
}

/// `path`, or the refusal of a symbolic link, which may lead out of the record.
fn linked(path: PathBuf) -> Result<PathBuf, String> {
    if fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(format!(
            "`{}` is a symbolic link: expected a file of the record's own",
            path.display()
        ));
    }
    Ok(path)
}

/// The bytes of model request `number`'s response in the record at `record`, as
/// [`Record::model_response`](super::Record::model_response) gives them.
pub(super) fn kept_response(record: &Path, number: u64, kept: &Value) -> Result<Vec<u8>, String> {
    let path = match kept {
        Value::Null => linked(record.join(format!("model/{number}.response")))?,
        Value::Object(kept) => kept_path(record, kept)?,
        other => {
            return Err(format!(
                "the run log's `kept.response` is {other}: expected an object"
            ));
        }
    };
    fs::read(&path).context(path.display())
}

/// The parts of the body of the request that `kept`, its run log's `kept.request`, names in the
/// record at `record`, as [`Record::kept_request`](super::Record::kept_request) says; `None` if
/// the record kept none of them apart, so its file holds the body as it went.
pub(super) fn kept_parts(
    record: &Path,
    kept: &Map<String, Value>,
) -> Result<Option<KeptRequest>, String> {
    // A record names the fields it kept apart, so a field that a client sent as a digest's text
    // stays as it was sent.
    let items = kept.get("items").and_then(|items| items["field"].as_str());
    let whole = kept.get("whole").and_then(Value::as_array);
    let whole: Vec<&str> = (whole.into_iter().flatten())
        .filter_map(Value::as_str)
        .collect();
    if items.is_none() && whole.is_empty() {
        return Ok(None);
    }
    let path = kept_path(record, kept)?;
    let bytes = fs::read(&path).context(path.display())?;
    let unkept = |what: &str| {
        format!(
            "{}: {what}: expected a body kept with its parts apart",
            path.display()
        )
    };
    let Ok(Value::Object(fields)) = serde_json::from_slice(&bytes) else {
        return Err(unkept("not a JSON object"));
    };
    let digest = |value: Value| match value {
        Value::String(digest) => Sha256Digest::try_from(digest).ok(),
        _ => None,
    };
    let mut request = KeptRequest {
        conversation: None,
        fields: Vec::new(),
    };
    for (field, value) in fields {
        if items == Some(field.as_str()) {
            let Value::Array(values) = value else {
                return Err(unkept(&format!("no array `{field}`")));
            };
            let parts = (values.into_iter())
                .map(|value| digest(value).map(KeptPart::Kept))
                .collect::<Option<_>>()
                .ok_or_else(|| unkept("an item that is no digest"))?;
            request.conversation = Some((field, parts));
        } else if whole.contains(&field.as_str()) {
            let part = digest(value).ok_or_else(|| unkept(&format!("`{field}` is no digest")))?;
            request.fields.push((field, KeptPart::Kept(part)));
        } else {
            request.fields.push((field, KeptPart::Sent(value)));
        }
    }
    if let Some(field) = items
        && request.conversation.is_none()
    {
        return Err(unkept(&format!("no array `{field}`")));
    }
    if let Some(field) =
        (whole.iter()).find(|&&field| !request.fields.iter().any(|(name, _)| name == field))
    {
        return Err(unkept(&format!("no field `{field}`")));
    }
    Ok(Some(request))
}

/// The bytes of the item of the record at `record` that `digest` names, as kept.
pub(super) fn kept_item(record: &Path, digest: &Sha256Digest) -> Result<Vec<u8>, String> {
    let path = record.join("model/items").join(digest.hex());
    fs::read(&path).context(path.display())
}

impl<C: Clock> Keep for Exchange<C> {
    fn dropped_headers(&self, names: &[String]) {
        *self.dropped_headers.borrow_mut() = names.to_vec();
    }

    fn request(&self, body: &[u8], apart: Option<Apart>) {
        let unsplit = || (Cow::Borrowed(body), Body::of(body, None, Vec::new()));
        let (kept, read) = (apart.and_then(|apart| self.kept_apart(body, apart)))
            .map_or_else(unsplit, |(rest, read)| (Cow::Owned(rest), read));
        *self.body.borrow_mut() = Some(read);
        self.add(&self.request, &kept);
    }

    fn response(&self, bytes: &[u8]) {
        if !bytes.is_empty() && self.first.get().is_none() {
            self.first.set(Some((self.clock.now(), None)));
        }
        self.add(&self.response, bytes);
    }

    fn first_output(&self) {
        let Some((first_byte, None)) = self.first.get() else {
            unreachable!(
                "the route told the stream's output began before any of its bytes, or twice, \
                 though it tells it once at most, after the bytes that hold it (`Keep`)"
            )
        };
        self.first.set(Some((first_byte, Some(self.clock.now()))));
    }
}
