//! The failures that a run's log states, folded from its records one at a time, and the failure
//! that a run without a result ended with: each as the fields its record carries, with where the
//! page marks it.
//!
//! A failure's subject is what the record that states it names, by that record's own fields, and
//! its reason is that record's, whole: nothing here is read out of a reason's words.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use runtime::{CallOf, CallStart, CallerName, Ending, Stopped};
use serde::Deserialize as _;
use serde_json::{Value, json};

use super::talk::{MODEL, Talk};
use crate::progress;

/// How many failed model requests of a run are held, the last ones: a run retries the requests
/// that fail, a lane's track on a page is about a thousand pixels wide, and a mark is ten.
const REQUEST_FAILURES: usize = 100;

/// A failure, as the page draws one.
struct Failure {
    /// What failed: its `kind`, and the ids that the record that states the failure names it by.
    subject: Value,
    reason: Option<String>,
    /// What the record keeps of the subject that the view serves, each piece with its `name`,
    /// its `path` below the run's address and its `bytes`.
    evidence: Vec<Value>,
    /// The subject's lane, by its name.
    lane: Option<String>,
    /// When it failed, in seconds since the run started.
    at: Option<f64>,
    /// The byte at which the log's record that states it starts.
    record: Option<u64>,
    role: Option<String>,
    /// The roles at the subject's two ends, for a subject that has two.
    between: Option<(String, String)>,
}

impl Failure {
    /// The failure of `subject`, on `lane`, that the log's `record`, which starts at `byte`,
    /// states, with the record's reason.
    fn stated(byte: u64, record: &Value, subject: Value, lane: String) -> Self {
        Self {
            subject,
            reason: record["event"]["reason"].as_str().map(str::to_owned),
            evidence: Vec::new(),
            lane: Some(lane),
            at: super::when(record),
            record: Some(byte),
            role: None,
            between: None,
        }
    }

    fn json(&self, state: &str) -> Value {
        let Self {
            subject,
            reason,
            evidence,
            lane,
            at,
            record,
            role,
            between,
        } = self;
        let between = between.as_ref().map(|(from, to)| json!([from, to]));
        json!({
            "state": state,
            "reason": reason,
            "at": at,
            "subject": subject,
            "evidence": evidence,
            "mark": { "lane": lane, "at": at, "record": record, "role": role, "between": between },
        })
    }
}

/// The failures of one run, to which each record of its log is `take`n in order.
#[derive(Default)]
pub(super) struct Failures {
    /// The failures of calls, agents, leases and shared sandboxes.
    ended: Vec<Failure>,
    requests: VecDeque<Failure>,
    /// The jobs whose calls the core asked to stop.
    stopped: BTreeSet<u64>,
    final_job: Option<u64>,
}

impl Failures {
    /// Adds one log `record`, which starts at `byte`, after `talk` took it.
    pub(super) fn take(&mut self, byte: u64, record: &Value, talk: &Talk) {
        for action in record["actions"].as_array().into_iter().flatten() {
            if let (Some("stop_call"), Some(job)) =
                (action["kind"].as_str(), action["job"].as_u64())
            {
                self.stopped.insert(job);
            }
            if let Some(CallStart {
                job,
                of: CallOf::Final,
                ..
            }) = CallStart::read(action)
            {
                self.final_job = Some(job);
            }
        }
        let event = &record["event"];
        // An answer that the route gave in place of the upstream's, or that states a failure.
        let answered = event["kind"] == "model_answered";
        if answered && (event["answer"]["kind"] == "refused" || !event["failed"].is_null()) {
            let Some(failure) = request(byte, record, talk) else {
                return;
            };
            if self.requests.len() == REQUEST_FAILURES {
                self.requests.pop_front();
            }
            self.requests.push_back(failure);
        } else {
            self.ended.extend(self.ended(byte, record, talk));
        }
    }

    /// The failure that `record` states of a call, an agent, a lease or a shared sandbox.
    fn ended(&self, byte: u64, record: &Value, talk: &Talk) -> Option<Failure> {
        let event = &record["event"];
        let id = |name: &str| event[name].as_u64();
        let stated = |subject, lane| Failure::stated(byte, record, subject, lane);
        match event["kind"].as_str()? {
            "call_failed" => {
                // A call that the core stopped ends as a failed one, and is none.
                let job = id("job").filter(|job| !self.stopped.contains(job))?;
                Some(Failure {
                    role: talk.role(CallerName::Job(job)),
                    between: talk.call_ends(job),
                    ..stated(json!({ "kind": "call", "job": job }), format!("job {job}"))
                })
            }
            "agent_failed" => {
                let caller = CallerName::deserialize(&event["caller"]).ok()?;
                let subject = json!({ "kind": "agent", "caller": caller });
                Some(Failure {
                    role: talk.role(caller),
                    ..stated(subject, progress::caller_lane(caller))
                })
            }
            "lease_failed" => {
                let lease = id("lease")?;
                Some(Failure {
                    role: talk.runners.get(&lease).cloned(),
                    between: talk.ends(lease),
                    ..stated(
                        json!({ "kind": "lease", "lease": lease }),
                        format!("lease {lease}"),
                    )
                })
            }
            "shared_failed" => {
                let shared = id("shared")?;
                let subject = json!({ "kind": "shared", "shared": shared });
                Some(Failure {
                    role: talk.role_at(shared),
                    ..stated(subject, format!("shared {shared}"))
                })
            }
            _ => None,
        }
    }

    /// The failure of each lane that failed, by the lane's name.
    pub(super) fn of_lanes(&self) -> BTreeMap<&str, Value> {
        let ended = self.ended.iter().filter_map(|failure| {
            let lane = failure.lane.as_deref()?;
            Some((lane, failure.json("failed")))
        });
        ended.collect()
    }

    /// The failures the log states, in the log's order.
    pub(super) fn json(&self) -> Value {
        let mut stated: Vec<&Failure> = self.ended.iter().chain(&self.requests).collect();
        stated.sort_by_key(|failure| failure.record);
        stated
            .iter()
            .map(|failure| failure.json("failed"))
            .collect()
    }
}

/// The failure of the model request that `record`, a failed answer, is of: its reason the
/// route's message, if it gave one, and what it left the response that the record keeps.
fn request(byte: u64, record: &Value, talk: &Talk) -> Option<Failure> {
    let event = &record["event"];
    let caller = CallerName::deserialize(&event["caller"]).ok()?;
    let request = event["request"].as_u64()?;
    let subject = json!({
        "kind": "request", "caller": caller, "request": request, "upstream": event["upstream"],
    });
    let role = talk.role(caller);
    let kept = event["kept"]["response"]["bytes"].as_u64().map(|bytes| {
        json!({
            "name": "The response, as kept",
            "path": format!("exchange/{request}/response"),
            "bytes": bytes,
        })
    });
    Some(Failure {
        reason: event["answer"]["message"].as_str().map(str::to_owned),
        evidence: kept.into_iter().collect(),
        between: role.clone().map(|role| (role, MODEL.to_owned())),
        role,
        ..Failure::stated(byte, record, subject, progress::caller_lane(caller))
    })
}

/// The failure that a run that ended as `ending` at `end` ended with: its final call's, if it
/// finished without a result and `stated` holds that call's failure, else the run's own, whose
/// subject is the run; `null` for a run with a result, and for one whose record states no ending.
pub(super) fn ended_json(ending: &Ending, end: Option<f64>, stated: &Failures) -> Value {
    let run = |subject: Value, reason: Option<String>| Failure {
        subject,
        reason,
        evidence: Vec::new(),
        lane: None,
        at: end,
        record: None,
        role: None,
        between: None,
    };
    match ending {
        Ending::Returned { .. } | Ending::Lost => Value::Null,
        Ending::NoResult(why) => {
            let state = "finished without a result";
            let last = stated.final_job.and_then(|job| {
                let mut failed = stated.ended.iter().rev();
                failed.find(|failure| failure.subject["job"].as_u64() == Some(job))
            });
            match last {
                Some(last) => last.json(state),
                None => run(json!({ "kind": "run" }), Some(why.clone())).json(state),
            }
        }
        Ending::Stopped(Stopped::Failed(reason)) => {
            run(json!({ "kind": "run" }), Some(reason.clone())).json("failed")
        }
        Ending::Stopped(Stopped::Interrupted(signal)) => {
            run(json!({ "kind": "run", "signal": signal }), None).json("interrupted")
        }
    }
}
