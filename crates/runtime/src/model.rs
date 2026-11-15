//! A role's model route as the runtime serves it, and the record's `model/` directory, where each
//! exchange that the route carries is kept.
//!
//! Request number `n` has `model/<n>.request`, its body as it went upstream or, if the route
//! refused it, as its client sent it, and `model/<n>.response`, the answer's bytes as they came; a
//! file that the exchange never wrote isn't there. A body that the route read is kept with its
//! conversation's items and its whole fields, such as a list of tools, each replaced by the digest
//! of its JSON text, which `model/items/<digest>` holds once for every request that sent it. The
//! run log's `model_answered` line says what an exchange kept ([`Exchange::kept`]), so that the
//! body can be rebuilt and checked against the digest of what went upstream.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
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

/// A role's route as served: each entry of it with the upstream that it reaches.
pub(super) struct Served {
    entries: Vec<(Route, Upstream)>,
    names: Vec<(Name, Api)>,
    failing: Mutex<Option<Failing>>,
}

#[derive(Debug)]
struct Failing {
    since: u64,
    last: (u64, Name, String),
}

impl fmt::Debug for Served {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The upstreams show nothing, as they hold the keys.
        (f.debug_struct("Served").field("names", &self.names)).finish_non_exhaustive()
    }
}

/// The servings of each role's route, with the models that the role's route allows, or why a role's
/// route can't be served: no upstream serves an entry's API, or serves a model that it allows.
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
    /// `None` if `servings` is empty.
    pub(super) fn new(servings: &[Serving]) -> Option<Self> {
        let entries = servings.iter().map(|serving| {
            let route = Route::new(serving.api, serving.models.clone(), &serving.headers);
            (
                route,
                Upstream::new(serving.api, &serving.url, &serving.key),
            )
        });
        let names = (servings.iter()).map(|serving| (serving.name.clone(), serving.api));
        (!servings.is_empty()).then(|| Self {
            entries: entries.collect(),
            names: names.collect(),
            failing: Mutex::new(None),
        })
    }

    /// Serves one model request, number `number`, with its `exchange` keeping it: `forwarding` is
    /// called once if the request goes upstream, and `answered` once, with the name and API of the
    /// entry that took it, if one did, and the answer.
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

    /// Notes, if the route is open, how upstream `name` answered request `number` in what the
    /// provider is failing the sandbox in: a failure extends it, a barred answer leaves it, and any
    /// other answer ends it.
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

    /// Checks that the sandbox's last request answered while the route was open got no failure from
    /// the model provider, if the agent's exit ended the work phase of a run that ended with
    /// `outcome` with no submission finalized. Fails, naming the upstream and the failure, if it
    /// did: the failure is then the deployment's, not the agent's result.
    pub(super) fn failed_last(&self, outcome: &Result<Outcome, Stopped>) -> Result<(), String> {
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

    /// Ends the route's every exchange in flight and every later one, unless it has ended.
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

/// How `answer` failed for a reason that isn't its request's: a status that says the deployment
/// can't serve it, such as 401, 429 or 500, or a response cut short, or a refusal with no answer.
/// `None` if it succeeded, or its status says the request was wrong, as 400 does.
fn failure(answer: &Answer) -> Option<String> {
    // The route fixes the path and the deployment the model, so a redirect, a 402 or a 404
    // (`model_not_found`) is the deployment's fault, as are authentication, rate and server errors;
    // the other 4xx are the request's.
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

/// The files of one model request that the route keeps as the exchange passes ([`Keep`]), on the
/// thread that serves it.
pub(super) struct Exchange<C> {
    clock: C,
    first: Cell<Option<(Duration, Option<Duration>)>>,
    dir: PathBuf,
    number: u64,
    request: Part,
    response: Part,
    body: RefCell<Option<Body>>,
    dropped_headers: RefCell<Vec<String>>,
    items: RefCell<Vec<File>>,
    failed: RefCell<Option<String>>,
}

struct Body {
    bytes: u64,
    sha256: Sha256Digest,
    items: Option<(&'static str, usize)>,
    whole: Vec<&'static str>,
}

impl Body {
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

    /// The `kept` of the exchange's `model_answered` line: `request` and `response`, each present
    /// if the exchange wrote the file and holding its `bytes`. A request that the route read has
    /// also its `body_bytes` and `body_sha256`, those of the whole body as it went upstream;
    /// `items`, the conversation's `field` and `count`; `whole`, the fields kept whole, in their
    /// API's order; and `dropped_headers`.
    pub(super) fn kept(&self) -> Value {
        let mut kept = json!({});
        for part in [&self.request, &self.response] {
            if part.file.borrow().is_some() {
                kept[part.name] = json!({ "bytes": part.bytes.get() });
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

    /// When the response's first byte arrived, and its first token of output if that began; `None`
    /// if no byte did. Both are times since the run started.
    pub(super) fn timed(&self) -> Option<(Duration, Option<Duration>)> {
        self.first.get()
    }

    /// Syncs the files, and returns what failed of keeping the exchange, if anything. Once a write
    /// has failed, the exchange keeps nothing more.
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
                .and_then(|()| File::create_new(self.dir.join(file_name(self.number, part.name))))
                .and_then(|made| file.insert(made).write_all(bytes)),
        };
        match written {
            Ok(()) => part.bytes.set(part.bytes.get() + bytes.len() as u64),
            Err(error) => *self.failed.borrow_mut() = Some(format!("{} {error}", part.name)),
        }
    }

    /// `body` with its conversation's items and its whole fields replaced by their digests, once
    /// each is kept in `model/items`. `None` if `body` is no JSON object or an item couldn't be
    /// kept, and the body is kept as it is.
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

    fn kept_item(&self, dir: &Path, value: &Value) -> Option<Value> {
        // The body went upstream as `serde_json` writes it, so each item is written alike, and the
        // body rebuilt from the items' text is the body that went.
        let text = value.to_string();
        let digest = sha256(text.as_bytes());
        let path = dir.join(digest.hex());
        if !path.try_exists().ok()? {
            // Kept once: by an earlier request, or by another caller's at the same time. It is
            // written under this exchange's own name, then linked to its digest's, which no later
            // link replaces, so no other request sees its name before all of it is written, as git
            // moves an object into place:
            // https://github.com/git/git/blob/fb255012acbcbe81189cb8014da0280b09c66ecc/object-file.c#L382-L457
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
    let body = rebuilt_body(record, number, &kept)?;
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

fn rebuilt_body(record: &Path, number: u64, kept: &Map<String, Value>) -> Result<Vec<u8>, String> {
    let Some(parts) = kept_parts(record, number, kept)? else {
        let path = kept_file(record, number, "request")?;
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

/// A request body kept with its parts apart, as the record has it.
#[derive(Debug, Clone, PartialEq)]
pub struct KeptRequest {
    /// The field that holds the conversation, and its items in order; `None` if the body was kept
    /// without splitting it.
    pub conversation: Option<(String, Vec<KeptPart>)>,
    /// The other fields, in the order they are kept.
    pub fields: Vec<(String, KeptPart)>,
}

/// A part of a kept request.
#[derive(Debug, Clone, PartialEq)]
pub enum KeptPart {
    /// Held in `model/items` under this digest.
    Kept(Sha256Digest),
    /// Kept in the request file as it was sent.
    Sent(Value),
}

fn file_name(number: u64, part: &str) -> String {
    format!("{number}.{part}")
}

pub(super) fn kept_file(record: &Path, number: u64, part: &str) -> Result<PathBuf, String> {
    linked(record.join("model").join(file_name(number, part)))
}

fn linked(path: PathBuf) -> Result<PathBuf, String> {
    // A link could lead a reader out of the record.
    if fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(format!(
            "`{}` is a symbolic link: expected a file of the record's own",
            path.display()
        ));
    }
    Ok(path)
}

pub(super) fn kept_response(record: &Path, number: u64) -> Result<Vec<u8>, String> {
    let path = kept_file(record, number, "response")?;
    fs::read(&path).context(path.display())
}

/// Request `number`'s kept parts, as the log's `kept.request`, `kept`, says them; `None` if it says
/// the body was kept whole.
pub(super) fn kept_parts(
    record: &Path,
    number: u64,
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
    let path = kept_file(record, number, "request")?;
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
