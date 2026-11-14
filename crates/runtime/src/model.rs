//! The model routes that a run serves: each role's ([`routes`]), and each sandbox's of a role
//! with one, as the run serves it ([`Served`]), by the rules of [`route`].

use std::cell::{Cell, RefCell};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use decision::{Outcome, Verdict, WorkEnd};
use gateway::IncomingModelRequest;
use route::{Answer, Keep, Route, Serving, Upstream};
use schema::ir::{ApiRoute, Name, Sha256Digest};
use schema::workflow::{ByRole, Sandbox, Workflow};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use stdx::{Context as _, Deadline, unpoisoned};

use crate::Stopped;

/// A sandbox's model route, as the run serves it: for each entry, the route that closes its
/// requests and the upstream that they reach, with the name that the deployment gives it.
pub(super) struct Served {
    entries: Vec<(Route, Upstream)>,
    names: Vec<Name>,
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
/// alone; none for a role without a route.
///
/// # Errors
///
/// Fails if a role's route has an entry that no serving of its API serves, or that allows a
/// model that its serving doesn't serve.
pub(super) fn routes(
    workflow: &Workflow,
    servings: &ByRole<Vec<Serving>>,
) -> Result<ByRole<Vec<Serving>>, String> {
    workflow.by_role(|(role, sandbox)| route(sandbox, servings.get(role)))
}

/// `sandbox`'s role's model route, as [`routes`] gives it.
fn route(sandbox: &Sandbox, servings: &[Serving]) -> Result<Vec<Serving>, String> {
    let Some(route) = &sandbox.model_route else {
        return Ok(Vec::new());
    };
    let role = &sandbox.name;
    let served = |entry: &ApiRoute| {
        let ApiRoute { api, models } = entry;
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
        })
    };
    route.as_slice().iter().map(served).collect()
}

/// The run log's note on which upstream serves each entry of the route of `sandbox`'s role,
/// `servings`, by its name, and the name that it takes for each model, if it has one.
pub(super) fn started(sandbox: &Sandbox, servings: &[Serving]) -> Option<String> {
    let entries: Vec<String> = servings
        .iter()
        .map(|serving| {
            let taken: Vec<String> = (serving.models.iter())
                .map(|(model, upstream_name)| format!("`{model}` as `{upstream_name}`"))
                .collect();
            format!(
                "`{}` to the upstream `{}`, which takes {}",
                serving.api,
                serving.name,
                taken.join(", ")
            )
        })
        .collect();
    (!entries.is_empty())
        .then(|| format!("`{}`'s model route: {}", sandbox.name, entries.join("; ")))
}

impl Served {
    /// The route that `servings` serve, a role's as [`routes`] gives it, with upstreams of its
    /// own, or `None` for a role without one.
    pub(super) fn new(servings: &[Serving]) -> Option<Self> {
        let entries = servings.iter().map(|serving| {
            let route = Route::new(serving.api, serving.models.clone());
            (route, Upstream::new(&serving.url, &serving.key))
        });
        let names = servings.iter().map(|serving| serving.name.clone());
        (!servings.is_empty()).then(|| Self {
            entries: entries.collect(),
            names: names.collect(),
            failing: Mutex::new(None),
        })
    }

    /// Answers `incoming`, the sandbox's model request numbered `number`, as [`route::serve`] does,
    /// keeping it in `exchange`. It gives `note` a note on a request that it forwards, before it
    /// does, and tells `answered` how it answered the request, before the answer's end is written.
    pub(super) fn serve(
        &self,
        (number, exchange): (u64, &Exchange),
        incoming: IncomingModelRequest,
        allowed: bool,
        by: Deadline,
        note: impl FnOnce(String),
        answered: impl FnOnce(Answer),
    ) {
        let forwarding = || note(format!("model request {number} goes upstream"));
        let answered = |entry: Option<usize>, answer| {
            if let Some(entry) = entry {
                self.bear(number, &self.names[entry], &answer);
            }
            answered(answer);
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

    /// The upstream that refused the route's key, if one has ([`Upstream::key_was_refused`]).
    pub(super) fn key_refused_by(&self) -> Option<&Name> {
        let refused = self
            .entries
            .iter()
            .position(|(_, upstream)| upstream.key_was_refused());
        refused.map(|entry| &self.names[entry])
    }

    /// Checks that the sandbox's last request answered while the route was open got no failure
    /// from the model provider, if the agent's exit ended the work phase of a run that ended with
    /// `outcome` with no submission finalized.
    ///
    /// # Errors
    ///
    /// Fails, naming the upstream and the failure, if it did.
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

    /// Whether [`Served::end`] has ended the route.
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
/// answer's body as `<n>.response`, each made as its first bytes come. A request whose body holds
/// a conversation the route read keeps each of its items once, in `items/<hex>`, the item's JSON
/// named by its SHA-256, and its body with that list of items as their digests, in order;
/// [`Record::model_request`](super::Record::model_request) rebuilds it.
pub(super) struct Exchange {
    dir: PathBuf,
    number: u64,
    request: Part,
    response: Part,
    /// The request's body, once it has come.
    body: Cell<Option<Body>>,
    /// The items it made, to sync at its end.
    items: RefCell<Vec<File>>,
    /// Why keeping it failed, if it did: it is then kept no further.
    failed: RefCell<Option<String>>,
}

/// What an exchange keeps of its request's body: its size, and the field whose items are kept
/// apart, with their count, if they are.
#[derive(Clone, Copy)]
struct Body {
    bytes: u64,
    items: Option<(&'static str, usize)>,
}

/// One file of an exchange's.
struct Part {
    name: &'static str,
    file: RefCell<Option<File>>,
    bytes: Cell<u64>,
}

impl Exchange {
    /// Exchange `number`, to keep in the record at `record`.
    pub(super) fn new(record: &Path, number: u64) -> Self {
        let part = |name| Part {
            name,
            file: RefCell::new(None),
            bytes: Cell::new(0),
        };
        Self {
            dir: record.join("model"),
            number,
            request: part("request"),
            response: part("response"),
            body: Cell::new(None),
            items: RefCell::new(Vec::new()),
            failed: RefCell::new(None),
        }
    }

    /// What the run log's `model_answered` says of where it is kept so far: `{"request": {"path":
    /// p, "bytes": b, "body_bytes": s, "items": {"field": f, "count": c}}, "response": {"path": p,
    /// "bytes": b}}`, each part once it has bytes, with the paths below the record: `s` the size of
    /// the request's body, and `items` the field whose `c` items are kept apart, if any.
    pub(super) fn kept(&self) -> Value {
        let mut kept = json!({});
        for part in [&self.request, &self.response] {
            if part.file.borrow().is_some() {
                let path = format!("model/{}.{}", self.number, part.name);
                kept[part.name] = json!({ "path": path, "bytes": part.bytes.get() });
            }
        }
        if let (Some(Body { bytes, items }), Some(request)) =
            (self.body.get(), kept.get_mut("request"))
        {
            request["body_bytes"] = json!(bytes);
            if let Some((field, count)) = items {
                request["items"] = json!({ "field": field, "count": count });
            }
        }
        kept
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

    /// Adds `bytes` to `part`'s file, which it makes if it has none yet.
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

    /// `body` with each item of its array `field` kept apart, once in the record ([`Exchange`]),
    /// and its place in the array holding its digest; `None` if `body` is no JSON object whose
    /// `field` is an array, or an item couldn't be kept.
    fn without_items(&self, body: &[u8], field: &str) -> Option<(Vec<u8>, usize)> {
        let Ok(Value::Object(mut fields)) = serde_json::from_slice(body) else {
            return None;
        };
        let Some(Value::Array(items)) = fields.get_mut(field) else {
            return None;
        };
        let dir = self.dir.join("items");
        fs::create_dir_all(&dir).ok()?;
        for item in items.iter_mut() {
            // The body went upstream as `serde_json` writes it, so each item is written alike,
            // and the body rebuilt from the items' text is the body that went.
            let text = item.to_string();
            let digest = Sha256Digest::from(<[u8; 32]>::from(Sha256::digest(text.as_bytes())));
            let path = dir.join(digest.hex());
            // Kept once: by an earlier request, or by another caller's at the same time.
            match File::create_new(&path) {
                Ok(mut file) => {
                    file.write_all(text.as_bytes()).ok()?;
                    self.items.borrow_mut().push(file);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(_) => return None,
            }
            *item = Value::String(digest.as_str().to_owned());
        }
        let count = items.len();
        Some((Value::Object(fields).to_string().into_bytes(), count))
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
    let path = record.join(kept.get("path").and_then(Value::as_str).unwrap_or_default());
    let bytes = fs::read(&path).context(path.display())?;
    let Some(field) = kept.get("items").and_then(|items| items["field"].as_str()) else {
        return Ok(bytes);
    };
    let unkept = |what: &str| {
        format!(
            "{}: {what}: expected a body kept with its items apart",
            path.display()
        )
    };
    let Ok(Value::Object(mut fields)) = serde_json::from_slice(&bytes) else {
        return Err(unkept("not a JSON object"));
    };
    let Some(Value::Array(items)) = fields.get_mut(field) else {
        return Err(unkept(&format!("no array `{field}`")));
    };
    for item in items.iter_mut() {
        let hex = item
            .as_str()
            .and_then(|digest| digest.strip_prefix("sha256:"));
        let Some(hex) = hex.filter(|hex| hex.bytes().all(|byte| byte.is_ascii_hexdigit())) else {
            return Err(unkept("an item that is no digest"));
        };
        let path = record.join("model/items").join(hex);
        let text = fs::read(&path).context(path.display())?;
        *item = serde_json::from_slice(&text)
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    Ok(Value::Object(fields).to_string().into_bytes())
}

impl Keep for Exchange {
    fn request(&self, body: &[u8], conversation: Option<&'static str>) {
        let split = conversation.and_then(|field| Some((field, self.without_items(body, field)?)));
        let (kept, items) = match &split {
            Some((field, (rest, count))) => (rest.as_slice(), Some((*field, *count))),
            None => (body, None),
        };
        let bytes = body.len() as u64;
        self.body.set(Some(Body { bytes, items }));
        self.add(&self.request, kept);
    }

    fn response(&self, bytes: &[u8]) {
        self.add(&self.response, bytes);
    }
}
