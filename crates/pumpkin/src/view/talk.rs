//! Who a run's roles talked to, folded from its log's records one at a time: the roles, with where
//! they ran and how they are routed, the traffic between them, each runner's commands and
//! transfers, and when each lane's model requests were answered.
//!
//! What a sandbox reaches other than by an op or its route, the log doesn't hold, and nor does
//! this.

use std::collections::BTreeMap;

use runtime::{CallOf, CallStart, CallerName, Place, RouteEntry};
use schema::ir::{self, Name};
use serde::Deserialize as _;
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
    /// Each grant that names a role, and the final call, as JSON: the role that may call, the
    /// role it reaches, and its kind.
    grants: Vec<Value>,
    edges: BTreeMap<(String, String, &'static str), Traffic>,
    /// Each lease's runner role, for the leases started so far.
    pub(super) runners: BTreeMap<u64, String>,
    work: BTreeMap<u64, Vec<Work>>,
    /// The times at which each lane's model requests were answered.
    pub(super) answered: BTreeMap<CallerName, Vec<f64>>,
    /// The times of the answers in `answered` that failed or were not forwarded.
    pub(super) failed: BTreeMap<CallerName, Vec<f64>>,
    reading: Reading,
}

impl Talk {
    /// The talk of a run before its log, with the roles of its `ir`, as placed in `places` and
    /// routed by `routes`, which is `None` where the run's routes are unknown.
    pub(super) fn new(
        ir: &ir::Workflow,
        places: &BTreeMap<String, Place>,
        routes: Option<&BTreeMap<String, Vec<RouteEntry>>>,
    ) -> Self {
        let roles = ir.roles.as_slice();
        let final_target = ir.final_call.target.as_str();
        let mut granted = BTreeMap::new();
        let mut grants = Vec::new();
        for role in roles {
            // Each grant that names a role: a job function its target, a session its runner; and
            // how the page names a call of it.
            let named: Vec<(&Name, &Name, &'static str)> = match &role.runtime {
                ir::Runtime::Agent(agent) => (agent.grants.iter())
                    .filter_map(|grant| match &grant.function {
                        ir::Function::Job(job) => Some((&grant.name, &job.target, "job call")),
                        ir::Function::Session(session) => {
                            Some((&grant.name, &session.runner, "lease"))
                        }
                        ir::Function::Now(_) => None,
                    })
                    .collect(),
                ir::Runtime::Target(ir::TargetRuntime { sessions, .. }) => (sessions.iter())
                    .map(|named| (&named.name, &named.session.runner, "lease"))
                    .collect(),
                ir::Runtime::Runner(_) => Vec::new(),
            };
            for (name, target, call) in named {
                let key = (role.name.to_string(), name.to_string());
                granted.insert(key, (target.to_string(), call));
                let kind = if call == "lease" { "lease" } else { "job" };
                grants.push(json!({ "from": role.name, "to": target, "kind": kind }));
            }
        }
        let mut agents = BTreeMap::new();
        for (index, role) in roles.iter().enumerate() {
            if matches!(role.runtime, ir::Runtime::Agent(_)) {
                let caller = if ir.final_call.input.is_some() {
                    CallerName::Member(index)
                } else {
                    CallerName::Agent
                };
                agents.insert(caller, role.name.to_string());
                grants.push(json!({ "from": role.name, "to": final_target, "kind": "final" }));
            }
        }
        let routes = |role: &ir::Role| {
            routes.map(|routes| {
                routes
                    .get(role.name.as_str())
                    .map_or(&[][..], Vec::as_slice)
            })
        };
        Self {
            roles: (roles.iter())
                .map(|role| role_json(role, final_target, places, routes(role)))
                .collect(),
            grants,
            edges: BTreeMap::new(),
            runners: BTreeMap::new(),
            work: BTreeMap::new(),
            answered: BTreeMap::new(),
            failed: BTreeMap::new(),
            reading: Reading {
                agents,
                final_target: final_target.to_owned(),
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

    fn role(&self, caller: CallerName) -> Option<String> {
        match caller {
            CallerName::Agent | CallerName::Member(_) => self.reading.agents.get(&caller).cloned(),
            CallerName::Job(job) => self.reading.jobs.get(&job).cloned(),
        }
    }

    fn ends(&self, lease: u64) -> Option<(String, String)> {
        let holder = self.reading.holders.get(&lease)?.clone();
        Some((holder, self.runners.get(&lease)?.clone()))
    }
}

struct Reading {
    agents: BTreeMap<CallerName, String>,
    final_target: String,
    /// Each grant's role, by its holder and name, and how the page names a call of it.
    granted: BTreeMap<(String, String), (String, &'static str)>,
    jobs: BTreeMap<u64, String>,
    holders: BTreeMap<u64, String>,
    directions: BTreeMap<(u64, u64), String>,
}

impl Talk {
    fn action(&mut self, at: f64, action: &Value, request: &Value) {
        if let Some(start) = CallStart::read(action) {
            self.call_started(start, action);
            return;
        }
        let lease = action["lease"].as_u64();
        match (action["kind"].as_str().unwrap_or_default(), lease) {
            ("start_lease", Some(lease)) => {
                let holder = CallerName::deserialize(&action["holder"])
                    .ok()
                    .and_then(|caller| self.role(caller));
                let Some(holder) = holder else { return };
                let grant = action["grant"].as_str().unwrap_or_default().to_owned();
                let Some((runner, _)) = self.reading.granted.get(&(holder.clone(), grant)).cloned()
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

    fn call_started(&mut self, start: CallStart, action: &Value) {
        let CallStart {
            job,
            of,
            caller,
            payload_bytes,
        } = start;
        let role = match of {
            CallOf::Function(grant) => {
                let Some(from) = caller.and_then(|caller| self.role(caller)) else {
                    return;
                };
                let Some((to, kind)) = self.reading.granted.get(&(from.clone(), grant)).cloned()
                else {
                    return;
                };
                self.edge(&from, &to, kind).add(Some(payload_bytes), None);
                to
            }
            CallOf::Final => {
                let to = self.reading.final_target.clone();
                if let Some(members) = action["selections"].as_array() {
                    for member in members {
                        if let (Some(from), Some(bytes)) = (
                            member["role"].as_str(),
                            member["selection"]["size"].as_u64(),
                        ) {
                            self.edge(from, &to, "final call").add(Some(bytes), None);
                        }
                    }
                } else if let Some(from) = self.role(CallerName::Agent) {
                    self.edge(&from, &to, "final call")
                        .add(Some(payload_bytes), None);
                }
                to
            }
        };
        self.reading.jobs.insert(job, role);
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
                let Ok(caller) = CallerName::deserialize(&event["caller"]) else {
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
                if event["answer"]["kind"] != "forwarded" || !event["failed"].is_null() {
                    self.failed.entry(caller).or_default().push(at);
                }
                self.answered.entry(caller).or_default().push(at);
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

    /// The roles, the grants and the edges as the page reads them.
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
        json!({ "roles": self.roles, "grants": self.grants, "edges": edges })
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
    role: &ir::Role,
    target: &str,
    places: &BTreeMap<String, Place>,
    routes: Option<&[RouteEntry]>,
) -> Value {
    let name = role.name.as_str();
    let (kind, has_route) = match &role.runtime {
        ir::Runtime::Agent(agent) => ("agent", agent.egress.model.is_some()),
        ir::Runtime::Target(target) => ("target", target.egress.model.is_some()),
        ir::Runtime::Runner(_) => ("runner", false),
    };
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
        "final_target": name == target,
        "routed": has_route,
        "place": place,
        "routes": routes,
    })
}
