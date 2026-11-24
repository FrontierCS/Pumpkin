//! The ops that a run brokered for each caller, as its log says of them: each call, lease command
//! and transfer, from when it arrived to when the work it started ended. An op that starts no
//! work, a now function's call or one refused, ends as it arrives; one whose work hasn't ended has
//! no end. The ops that only ask how things stand, or open and close a lease, are left out.

use std::collections::BTreeMap;

use runtime::CallerName;
use schema::ir::Name;
use serde::Deserialize as _;
use serde_json::{Value, json};

use super::nanos;
use crate::progress;

#[derive(Default)]
pub(in crate::view) struct Ops {
    ops: Vec<Op>,
    /// The op that started each piece of work, by the work.
    by_work: BTreeMap<Work, usize>,
}

struct Op {
    caller: CallerName,
    number: u64,
    kind: &'static str,
    /// Its grant or function, or its lease as `lease <n>`.
    name: Option<String>,
    start_ns: Option<u64>,
    end_ns: Option<u64>,
    result: Option<String>,
    failed: bool,
}

/// A piece of work that an op starts, as the log names it.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Work {
    Call(u64),
    Command(u64, u64),
    Transfer(u64, u64),
}

const KINDS: [&str; 5] = ["call", "now", "lease_run", "lease_put", "lease_get"];

impl Ops {
    pub(in crate::view) fn take(&mut self, record: &Value) {
        let event = &record["event"];
        // As [`super::Exchanges::take`] times a line.
        let ns = nanos(&record["observed_ns"]).or_else(|| nanos(&record["at_ns"]));
        let number = |name: &str| event[name].as_u64();
        let (work, result, failed) = match event["kind"].as_str() {
            Some("op_arrived") => return self.arrived(event, &record["actions"], ns),
            Some("call_returned") => {
                let Ok(values) = runtime::returned_values(&event["returned"]) else {
                    return;
                };
                (number("job").map(Work::Call), returned_text(&values), false)
            }
            Some("call_failed") => {
                let why = event["reason"].as_str().unwrap_or_default();
                (
                    number("job").map(Work::Call),
                    progress::call_failed(why),
                    true,
                )
            }
            Some("command_ended") => {
                let end = &event["end"];
                let (result, failed) = match end["exited"]["code"].as_i64() {
                    Some(code) => (format!("exit {code}"), code != 0),
                    None => (end_text(end), true),
                };
                let work = number("lease").zip(number("command"));
                (
                    work.map(|(lease, n)| Work::Command(lease, n)),
                    result,
                    failed,
                )
            }
            Some("transfer_ended") => {
                let moved = &event["end"]["moved"];
                let (result, failed) = match (moved["bytes"].as_u64(), moved["entries"].as_u64()) {
                    (Some(bytes), Some(entries)) => {
                        (format!("moved {bytes} bytes, {entries} entries"), false)
                    }
                    _ => (end_text(&event["end"]), true),
                };
                let work = number("lease").zip(number("transfer"));
                (
                    work.map(|(lease, n)| Work::Transfer(lease, n)),
                    result,
                    failed,
                )
            }
            _ => return,
        };
        let Some(op) = work.and_then(|work| self.by_work.remove(&work)) else {
            return;
        };
        let op = &mut self.ops[op];
        op.end_ns = ns;
        op.result = Some(result);
        op.failed = failed;
    }

    fn arrived(&mut self, event: &Value, actions: &Value, ns: Option<u64>) {
        let request = &event["request"];
        let (Some(kind), Some(caller), Some(op)) = (
            KINDS.into_iter().find(|kind| request["kind"] == *kind),
            CallerName::deserialize(&event["caller"]).ok(),
            event["op"].as_u64(),
        ) else {
            return;
        };
        let actions = actions.as_array().map_or(&[][..], Vec::as_slice);
        let reply = (actions.iter())
            .find(|action| action["kind"] == "reply")
            .map_or(&Value::Null, |action| &action["reply"]);
        let name = match request["lease"].as_u64() {
            Some(lease) => Some(format!("lease {lease}")),
            None => request["grant"].as_str().map(str::to_owned),
        };
        // A call's job may start later, once a slot frees; a lease's work starts at once.
        let work = if reply["kind"] == "call" {
            reply["job"].as_u64().map(Work::Call)
        } else {
            actions.iter().find_map(|action| {
                let lease = action["lease"].as_u64()?;
                match action["kind"].as_str()? {
                    "run_command" => Some(Work::Command(lease, action["command"].as_u64()?)),
                    "transfer" => Some(Work::Transfer(lease, action["transfer"].as_u64()?)),
                    _ => None,
                }
            })
        };
        let (end_ns, result, failed) = match (&work, reply["kind"].as_str()) {
            (Some(_), _) => (None, None, false),
            (None, Some("refused")) => {
                let why = reply["message"].as_str().unwrap_or_default();
                (ns, Some(format!("refused: {why}")), true)
            }
            // A now function's call, which gives what its body returned.
            (None, _) => {
                let returned = runtime::returned_values(&request["evaluated"]["returned"]);
                let Ok(values) = returned else {
                    return;
                };
                (ns, Some(returned_text(&values)), false)
            }
        };
        if let Some(work) = work {
            self.by_work.insert(work, self.ops.len());
        }
        self.ops.push(Op {
            caller,
            number: op,
            kind,
            name,
            start_ns: ns,
            end_ns,
            result,
            failed,
        });
    }

    /// Whether any op is `caller`'s.
    pub(super) fn names(&self, caller: CallerName) -> bool {
        self.ops.iter().any(|op| op.caller == caller)
    }

    /// `caller`'s ops, as columns of equal length, in the order they arrived.
    pub(in crate::view) fn json(&self, caller: CallerName) -> Value {
        let ops: Vec<&Op> = self.ops.iter().filter(|op| op.caller == caller).collect();
        let time = |ns: Option<u64>| ns.map(|ns| ns.to_string());
        json!({
            "caller": caller,
            "ops": {
                "op": ops.iter().map(|op| op.number).collect::<Vec<_>>(),
                "kind": ops.iter().map(|op| op.kind).collect::<Vec<_>>(),
                "name": ops.iter().map(|op| op.name.as_deref()).collect::<Vec<_>>(),
                "start_ns": ops.iter().map(|op| time(op.start_ns)).collect::<Vec<_>>(),
                "end_ns": ops.iter().map(|op| time(op.end_ns)).collect::<Vec<_>>(),
                "result": ops.iter().map(|op| op.result.as_deref()).collect::<Vec<_>>(),
                "failed": ops.iter().map(|op| op.failed).collect::<Vec<_>>(),
            },
        })
    }
}

/// What a body returned, in words: `returned`, then each value by its name.
fn returned_text(values: &[(Name, Value)]) -> String {
    match progress::shown_values(values) {
        shown if shown.is_empty() => "returned".to_owned(),
        shown => format!("returned {shown}"),
    }
}

/// An end that isn't the one hoped for, in words: `timed_out`, or `failed: <why>`.
fn end_text(end: &Value) -> String {
    match end {
        Value::String(kind) => kind.clone(),
        Value::Object(fields) => (fields.iter().next())
            .map(|(kind, why)| match why.as_str() {
                Some(why) => format!("{kind}: {why}"),
                None => kind.clone(),
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
}
