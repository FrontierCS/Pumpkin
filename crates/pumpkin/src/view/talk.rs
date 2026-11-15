//! Who a run's roles talked to, folded from its log's records one at a time: the roles, with where
//! they ran and how they are routed, the traffic between them, each runner's commands and
//! transfers, and when each lane's model requests were answered.
//!
//! What a sandbox reaches other than by an op or its route, the log doesn't hold, and nor does
//! this.

use std::collections::BTreeMap;

use runtime::{Place, RouteEntry};
use serde_json::{Value, json};

/// The model upstream, which every role with a route reaches: an end of edges, but no role.
pub(super) const MODEL: &str = "model";

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

struct Work {
    kind: String,
    label: String,
    from: f64,
    to: Option<f64>,
    end: Option<Value>,
}

/// The talk of one run, to which each record of its log is `take`n in order.
pub(super) struct Talk {
    roles: Vec<Value>,
    edges: BTreeMap<(String, String, &'static str), Traffic>,
    /// Each lease's runner role, for the leases started so far.
    pub(super) runners: BTreeMap<u64, String>,
    work: BTreeMap<u64, Vec<Work>>,
    /// The times at which each lane's model requests were answered.
    pub(super) answered: BTreeMap<String, Vec<f64>>,
    /// The times of the answers in `answered` that failed or were not forwarded.
    pub(super) failed: BTreeMap<String, Vec<f64>>,
    reading: Reading,
}

impl Talk {
    /// The talk of a run before its log, with the roles of its `ir`, as placed in `places` and
    /// routed by `routes`, which is `None` where the run's routes are unknown.
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

    /// Adds one log `record`.
    pub(super) fn take(&mut self, record: &Value) {
        let at = super::when(record).unwrap_or_default();
        let request = &record["event"]["request"];
        for action in record["actions"].as_array().into_iter().flatten() {
            self.action(at, action, request);
        }
        self.event(at, &record["event"]);
    }

    fn role(&self, caller: &str) -> Option<String> {
        match caller.strip_prefix("job-") {
            Some(job) => self.reading.jobs.get(&job.parse().ok()?).cloned(),
            None => (caller == "agent").then(|| self.reading.agent.clone()),
        }
    }

    fn ends(&self, lease: u64) -> Option<(String, String)> {
        let holder = self.reading.holders.get(&lease)?.clone();
        Some((holder, self.runners.get(&lease)?.clone()))
    }
}

struct Reading {
    agent: String,
    score: String,
    granted: BTreeMap<(String, String), String>,
    jobs: BTreeMap<u64, String>,
    holders: BTreeMap<u64, String>,
    directions: BTreeMap<(u64, u64), String>,
}

impl Talk {
    fn action(&mut self, at: f64, action: &Value, request: &Value) {
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
                let argv = request["argv"].as_array().into_iter().flatten();
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
                // A put's request names its tree, a get's the path it takes from.
                let from = request["tree"]
                    .as_str()
                    .or_else(|| request["from"].as_str());
                let (from, to) = (from, request["to"].as_str());
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

    /// Ends the earliest of `lease`'s work of `kind` that runs: a lease's runner runs one command
    /// or transfer at a time.
    fn ended(&mut self, lease: u64, kind: &str, at: f64, end: &Value) {
        let work = self.work.entry(lease).or_default().iter_mut();
        let mut running = work.filter(|work| work.kind == kind && work.to.is_none());
        if let Some(work) = running.next() {
            work.to = Some(at);
            work.end = Some(end.clone());
        }
    }

    /// The roles and edges as the page reads them.
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

    /// The commands and transfers of `lease`'s runner so far, with when each began and ended.
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
