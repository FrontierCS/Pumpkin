//! Who talks to whom in a run, as its log records it: the workflow's roles, from its IR, and
//! what went between them, by kind, with how many and how many bytes where the log gives them:
//! each judge call and its payload, the submission the final judge scores, each lease, its
//! commands and its transfers' trees, each way, and each model request and its answer. And what
//! each lease's runner did, command by command, for the lanes of the run's sandboxes.
//!
//! What a sandbox reaches other than by an op or its route, the log doesn't hold, and nor does
//! this.

use std::collections::BTreeMap;

use runtime::{Place, RouteEntry};
use serde_json::{Value, json};

/// The model upstream, which every role with a route reaches: an end of edges, but no role.
pub(super) const MODEL: &str = "model";

/// What went along an edge: how many, and the bytes each way where the log gives them.
#[derive(Default)]
struct Traffic {
    count: u64,
    bytes: Option<u64>,
    back_bytes: Option<u64>,
}

impl Traffic {
    fn add(&mut self, bytes: Option<u64>, back_bytes: Option<u64>) {
        self.count += 1;
        let sum = |total: Option<u64>, more: Option<u64>| match (total, more) {
            (total, None) => total,
            (total, Some(more)) => Some(total.unwrap_or(0) + more),
        };
        self.bytes = sum(self.bytes, bytes);
        self.back_bytes = sum(self.back_bytes, back_bytes);
    }
}

/// A command or transfer of a lease's runner, from when the core started it until it ended, as
/// the log says: its end as the log gives it.
struct Work {
    kind: String,
    label: String,
    from: f64,
    to: Option<f64>,
    end: Option<Value>,
}

/// What the log of a run says of who talks to whom, as far as it was taken.
pub(super) struct Talk {
    /// Each role's name, kind and place, as JSON, in the IR's order.
    roles: Vec<Value>,
    edges: BTreeMap<(String, String, &'static str), Traffic>,
    /// The role of each lease's runner, by lease.
    pub(super) runners: BTreeMap<u64, String>,
    /// Each lease's work, by lease, in the order the core started it.
    work: BTreeMap<u64, Vec<Work>>,
    /// When each caller's model requests were answered, by its lane: `agent` or `job <n>`.
    pub(super) answered: BTreeMap<String, Vec<f64>>,
    /// Of those, the answers that failed: not forwarded, or stating a failure.
    pub(super) failed: BTreeMap<String, Vec<f64>>,
    reading: Reading,
}

impl Talk {
    /// Who talks to whom in the run whose IR is `ir`, whose roles are placed as `places` says,
    /// and whose roles' model routes are `routes`, if its build kept them, before any of its log
    /// is read.
    pub(super) fn new(
        ir: &Value,
        places: &BTreeMap<String, Place>,
        routes: Option<&BTreeMap<String, Vec<RouteEntry>>>,
    ) -> Self {
        let roles = ir["roles"].as_array().map_or(&[][..], Vec::as_slice);
        let score = ir["score"]["role"].as_str().unwrap_or_default();
        let mut granted = BTreeMap::new();
        for role in roles {
            for grant in role["grants"].as_array().into_iter().flatten() {
                for kind in ["judge", "lease"] {
                    if let (Some(holder), Some(name), Some(target)) = (
                        role["name"].as_str(),
                        grant[kind]["name"].as_str(),
                        grant[kind]["role"].as_str(),
                    ) {
                        granted.insert((holder.to_owned(), name.to_owned()), target.to_owned());
                    }
                }
            }
        }
        let routes = |role: &Value| {
            let name = role["name"].as_str().unwrap_or_default();
            routes.map(|routes| routes.get(name).map_or(&[][..], Vec::as_slice))
        };
        Self {
            roles: (roles.iter())
                .map(|role| role_json(role, score, places, routes(role)))
                .collect(),
            edges: BTreeMap::new(),
            runners: BTreeMap::new(),
            work: BTreeMap::new(),
            answered: BTreeMap::new(),
            failed: BTreeMap::new(),
            reading: Reading {
                agent: (roles.iter())
                    .find(|role| role["runtime"]["agent"].is_object())
                    .and_then(|role| role["name"].as_str())
                    .unwrap_or_default()
                    .to_owned(),
                score: score.to_owned(),
                granted,
                jobs: BTreeMap::new(),
                holders: BTreeMap::new(),
                directions: BTreeMap::new(),
            },
        }
    }

    /// Takes in `record`, the log's next record.
    pub(super) fn take(&mut self, record: &Value) {
        let at = super::when(record).unwrap_or_default();
        for action in record["actions"].as_array().into_iter().flatten() {
            self.action(at, action);
        }
        self.event(at, &record["event"]);
    }

    /// The role of `caller`, as the log names one: `agent` or `job-<n>`.
    fn role(&self, caller: &str) -> Option<String> {
        match caller.strip_prefix("job-") {
            Some(job) => self.reading.jobs.get(&job.parse().ok()?).cloned(),
            None => (caller == "agent").then(|| self.reading.agent.clone()),
        }
    }

    /// The roles of `lease`'s holder and its runner.
    fn ends(&self, lease: u64) -> Option<(String, String)> {
        let holder = self.reading.holders.get(&lease)?.clone();
        Some((holder, self.runners.get(&lease)?.clone()))
    }
}

/// What a [`Talk`] needs to know of the records to come.
struct Reading {
    /// The agent's role, and the score judge's.
    agent: String,
    score: String,
    /// The role each grant reaches, by the role that holds it and its name.
    granted: BTreeMap<(String, String), String>,
    /// The role of each judge call, by job.
    jobs: BTreeMap<u64, String>,
    /// The role of each lease's holder, by lease.
    holders: BTreeMap<u64, String>,
    /// Each transfer's direction, by lease and transfer.
    directions: BTreeMap<(u64, u64), String>,
}

impl Talk {
    fn action(&mut self, at: f64, action: &Value) {
        let lease = action["lease"].as_u64();
        match (action["kind"].as_str().unwrap_or_default(), lease) {
            ("start_judge", _) => {
                let Some(job) = action["job"].as_u64() else {
                    return;
                };
                let (role, kind) = match action["grant"].as_str() {
                    None => (Some(self.reading.score.clone()), "submission"),
                    Some(grant) => {
                        let grant = (self.reading.agent.clone(), grant.to_owned());
                        (self.reading.granted.get(&grant).cloned(), "judge call")
                    }
                };
                let Some(role) = role else { return };
                let size = action["payload"]["size"].as_u64();
                self.edge(&self.reading.agent.clone(), &role, kind)
                    .add(size, None);
                self.reading.jobs.insert(job, role);
            }
            ("start_lease", Some(lease)) => {
                let holder = action["holder"]
                    .as_str()
                    .and_then(|holder| self.role(holder));
                let grant = action["grant"].as_str().unwrap_or_default().to_owned();
                let Some(holder) = holder else { return };
                let Some(runner) = self.reading.granted.get(&(holder.clone(), grant)).cloned()
                else {
                    return;
                };
                self.edge(&holder, &runner, "lease").add(None, None);
                self.runners.insert(lease, runner);
                self.reading.holders.insert(lease, holder);
            }
            ("run_command", Some(lease)) => {
                let argv = action["argv"].as_array().into_iter().flatten();
                let argv: Vec<&str> = argv.filter_map(Value::as_str).collect();
                if let Some((holder, runner)) = self.ends(lease) {
                    self.edge(&holder, &runner, "command").add(None, None);
                }
                self.started(lease, "command", argv.join(" "), at);
            }
            ("transfer", Some(lease)) => {
                let direction = action["direction"].as_str().unwrap_or_default();
                if let Some(transfer) = action["transfer"].as_u64() {
                    (self.reading.directions).insert((lease, transfer), direction.to_owned());
                }
                let (from, to) = (action["from"].as_str(), action["to"].as_str());
                let label = format!("{} → {}", from.unwrap_or("?"), to.unwrap_or("?"));
                self.started(lease, direction, label, at);
            }
            _ => {}
        }
    }

    fn event(&mut self, at: f64, event: &Value) {
        let lease = event["lease"].as_u64();
        match (event["kind"].as_str().unwrap_or_default(), lease) {
            ("command_ended", Some(lease)) => self.ended(lease, "command", at, &event["end"]),
            ("transfer_ended", Some(lease)) => {
                let transfer = event["transfer"].as_u64().unwrap_or(u64::MAX);
                let direction = self.reading.directions.get(&(lease, transfer)).cloned();
                let direction = direction.unwrap_or_default();
                self.ended(lease, &direction, at, &event["end"]);
                let bytes = event["end"]["moved"]["bytes"].as_u64();
                let Some((holder, runner)) = self.ends(lease) else {
                    return;
                };
                let (from, to) = match direction.as_str() {
                    "get" => (runner, holder),
                    _ => (holder, runner),
                };
                self.edge(&from, &to, "tree").add(bytes, None);
            }
            ("model_answered", _) => {
                // A record of a build that kept no exchanges names no caller.
                let Some(caller) = event["caller"].as_str() else {
                    return;
                };
                let Some(role) = self.role(caller) else {
                    return;
                };
                let kept = &event["kept"];
                let (sent, back) = (
                    kept["request"]["bytes"].as_u64(),
                    kept["response"]["bytes"].as_u64(),
                );
                self.edge(&role, MODEL, "model request").add(sent, back);
                let lane = caller.replace('-', " ");
                if event["answer"]["kind"] != "forwarded" || !event["failed"].is_null() {
                    self.failed.entry(lane.clone()).or_default().push(at);
                }
                self.answered.entry(lane).or_default().push(at);
            }
            _ => {}
        }
    }
}

impl Talk {
    fn edge(&mut self, from: &str, to: &str, kind: &'static str) -> &mut Traffic {
        let key = (from.to_owned(), to.to_owned(), kind);
        self.edges.entry(key).or_default()
    }

    fn started(&mut self, lease: u64, kind: &str, label: String, at: f64) {
        self.work.entry(lease).or_default().push(Work {
            kind: kind.to_owned(),
            label,
            from: at,
            to: None,
            end: None,
        });
    }

    /// Ends the earliest of `lease`'s work of `kind` that runs: a lease's runner runs one
    /// command or transfer at a time.
    fn ended(&mut self, lease: u64, kind: &str, at: f64, end: &Value) {
        let work = self.work.entry(lease).or_default().iter_mut();
        let mut running = work.filter(|work| work.kind == kind && work.to.is_none());
        if let Some(work) = running.next() {
            work.to = Some(at);
            work.end = Some(end.clone());
        }
    }

    /// The roles and the edges, as JSON: `{"roles": [...], "edges": [{"from", "to", "kind",
    /// "count", "bytes", "back_bytes"}]}`.
    pub(super) fn json(&self) -> Value {
        let edges: Vec<Value> = (self.edges.iter())
            .map(|((from, to, kind), traffic)| {
                json!({
                    "from": from,
                    "to": to,
                    "kind": kind,
                    "count": traffic.count,
                    "bytes": traffic.bytes,
                    "back_bytes": traffic.back_bytes,
                })
            })
            .collect();
        json!({ "roles": self.roles, "edges": edges })
    }

    /// Lease `lease`'s work, as JSON, for its lane.
    pub(super) fn work_json(&self, lease: u64) -> Value {
        let work = self.work.get(&lease).map_or(&[][..], Vec::as_slice);
        let work: Vec<Value> = (work.iter())
            .map(|work| {
                json!({
                    "kind": work.kind,
                    "label": work.label,
                    "from": work.from,
                    "to": work.to,
                    "end": work.end,
                })
            })
            .collect();
        Value::Array(work)
    }
}

/// `role` of the IR as the page shows it: its name, its kind, whether it is the score's, whether
/// it has a model route, where `places` places it, and its model route's entries as `routes`
/// says, or `"unknown"` if the record's build kept none.
fn role_json(
    role: &Value,
    score: &str,
    places: &BTreeMap<String, Place>,
    routes: Option<&[RouteEntry]>,
) -> Value {
    let name = role["name"].as_str().unwrap_or_default();
    let runtime = role["runtime"].as_object();
    let kind = runtime.and_then(|runtime| runtime.keys().next().cloned());
    let place = (places.get(name)).map(|Place { provider, class }| format!("{provider}/{class}"));
    let routes = routes.map_or(json!("unknown"), |entries| {
        let entries = entries.iter().map(|entry| {
            let RouteEntry {
                api,
                upstream,
                models,
            } = entry;
            json!({ "api": api, "upstream": upstream, "models": models })
        });
        Value::Array(entries.collect())
    });
    json!({
        "name": name,
        "kind": kind,
        "score": name == score,
        "routed": !role["egress"]["model"].is_null(),
        "place": place,
        "routes": routes,
    })
}
