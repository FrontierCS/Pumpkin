//! The JSON that the view answers with for a run and its log: the forms of what `runtime` reads
//! from a record and of what [`super::talk`] folds from its log.

use std::collections::BTreeMap;
use std::time::Duration;

use runtime::{
    CallerName, Cost, Ending, ModelUse, Progress, Readout, Record, SandboxStart, StartPhase, Tokens,
};
use serde_json::{Value, json};

use super::failure::{self, Failures};
use super::talk::Talk;
use super::{Held, when};
use crate::progress::driver::{self, UnitUse};
use crate::progress::{self, Listed, Row};
use crate::{controller, next};

/// How many marks at most a lane's answers and failures are drawn as across the run's time: a
/// lane's track on a page is about a thousand pixels wide, and a run makes thousands of requests.
const LANE_TICKS: u32 = 500;

/// How many phases of a sandbox's start a lane gives at most: its provider's first ones. A provider
/// may time any number, and each is a row of the start's card on the page.
// The card's other 11 rows and 16 phases, at 21 px a row, make a card of about 570 px, which a
// laptop's window holds.
const START_PHASES: usize = 16;

/// A log line as the page reads it: its fields, each string cut, with the byte it starts at and
/// its `when`. A line that is no JSON object comes as `unread` text.
pub(super) fn record_json(byte: u64, line: &[u8]) -> Value {
    if let Ok(Value::Object(fields)) = serde_json::from_slice(line) {
        let mut record = cut(Value::Object(fields));
        record["when"] = json!(when(&record));
        record["byte"] = json!(byte);
        return record;
    }
    let text = cut(Value::String(String::from_utf8_lossy(line).into_owned()));
    json!({ "byte": byte, "unread": text })
}

/// `value` with each string cut as [`cut_text`] cuts it.
pub(super) fn cut(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(cut_text(text)),
        Value::Array(values) => Value::Array(values.into_iter().map(cut).collect()),
        Value::Object(fields) => Value::Object(
            (fields.into_iter())
                .map(|(name, value)| (name, cut(value)))
                .collect(),
        ),
        Value::Null | Value::Bool(_) | Value::Number(_) => value,
    }
}

/// A text past [`progress::TEXT_SHOWN_BYTES`] cut there, at a character's start, and marked with
/// its whole length; a shorter one as it is.
pub(super) fn cut_text(text: String) -> String {
    if text.len() <= progress::TEXT_SHOWN_BYTES {
        return text;
    }
    let shown = progress::clipped(&text, progress::TEXT_SHOWN_BYTES);
    format!("{shown} [{} bytes in all]", text.len())
}

/// The row of the run list for a run that has not ended.
pub(super) fn live_row(held: &Held, at: &next::At) -> Value {
    let progress = held.readout.progress.clone();
    let model = progress.as_ref().map(|progress| progress.model.clone());
    let listed = Listed::of(&held.record, held.task.clone(), None, progress);
    let id = held.record.id();
    let mut row = listed_json(id, &Row::of(id, &listed, crate::unix_now(), at));
    row["requests"] = json!(model.as_ref().map(|model| model.requests));
    row["tokens"] = json!(
        model
            .as_ref()
            .map(|model| tokens_json(&model.tokens, model.requests))
    );
    if let Some(started_over) = &held.started_over {
        row["started_over"] = json!(started_over);
    }
    row
}

/// The row of the run list for run `id`, as `ls` shows `row`.
pub(super) fn listed_json(id: &str, row: &Row) -> Value {
    let progress::Columns {
        state,
        time,
        bound,
        usd,
        rest,
    } = &row.cells;
    json!({
        "id": id,
        "live": row.live(),
        "state": state,
        "has_result": row.has_result(),
        "time": time,
        "bound": bound,
        "usd": usd,
        "rest": rest,
    })
}

/// What a live run's job's unit uses, each figure as `ls` says it, by its name.
pub(super) fn unit_json(used: &UnitUse) -> Value {
    let [(_, cpu), (_, memory)] = driver::unit_figures(used);
    json!({ "cpu": cpu, "memory": memory })
}

/// A run's tokens, where `input` and `output` are `null` if requests were made and no response
/// reported its tokens, which is not none used.
pub(super) fn tokens_json(tokens: &Tokens, requests: u64) -> Value {
    let Tokens {
        reported,
        input,
        output,
        cached,
        cache_write: _,
        reasoning: _,
    } = tokens;
    let known = *reported > 0 || requests == 0;
    json!({
        "input": known.then_some(input),
        "output": known.then_some(output),
        "cached": cached,
    })
}

/// The values a run's result shows, each by its name, in their order, and how its work phase
/// ended, for a run that finished with a result; `null` for any other.
fn result_json(ending: Option<&Ending>) -> Value {
    let Some(Ending::Returned { values }) = ending else {
        return Value::Null;
    };
    let values: Vec<Value> = (values.iter())
        .map(|(name, value)| json!({ "name": name, "value": value }))
        .collect();
    json!({ "values": values })
}

fn how(ending: &Ending) -> String {
    match ending {
        Ending::Returned { values } => crate::ending::returned(values),
        Ending::NoResult(why) => why.clone(),
        Ending::Stopped(stopped) => stopped.to_string(),
        Ending::Lost => "is lost: its driver ended without writing outcome.json".to_owned(),
    }
}

#[expect(clippy::too_many_lines, reason = "a field of the reply after another")]
/// The reply for one run: its state and verdict, times, lanes, talk, model use, cost and log end.
///
/// `again` adds the command that starts the run over.
pub(super) fn run_json(held: &Held, at: &next::At, again: bool) -> Value {
    let record = &held.record;
    let id = record.id();
    let readout = &held.readout;
    let summary = progress::summary_of(record, readout);
    let Readout { progress, unread } = readout;
    let now = crate::unix_now();
    let listed = Listed::of(
        record,
        held.task.clone(),
        held.ending.clone(),
        progress.clone(),
    );
    let row = Row::of(id, &listed, now, at);
    let verdict = match (&held.ending, &progress) {
        (Some(ending), _) => format!("Run {id} {}", how(ending)),
        (None, None) => format!("Run {id} is publishing what its sandboxes run"),
        (None, Some(_)) => {
            let progress::Columns {
                state,
                time,
                bound,
                rest,
                ..
            } = &row.cells;
            // A sentence, as the page's words are (`page/h.js`): its first word, the state, takes a
            // capital.
            let (first, others) = state.split_at(1);
            format!(
                "{}{others} for {time} {bound}: {rest}",
                first.to_uppercase()
            )
        }
    };
    // The stage of a run that goes on, as `ls` shows it in its columns: the stage's name, how
    // long it has lasted, and ` of ` how long it may, or nothing where it has no bound.
    let phase = held.ending.is_none().then(|| {
        let progress::Columns {
            state, time, bound, ..
        } = &row.cells;
        json!({ "name": state, "time": time, "bound": bound })
    });
    let state = match (&held.ending, &progress) {
        (None, None) => "publishing",
        (Some(_), _) | (None, Some(_)) => row.state().name(),
    };
    let times = progress.as_ref().map(|progress| {
        let live_at = (held.ending.is_none()).then(|| now.since(progress.started));
        json!({
            "started_unix": progress.started.since_epoch().as_secs_f64(),
            "hard_end": progress.hard_end.as_secs_f64(),
            "last": progress.last.as_secs_f64(),
            "end": progress.end.map(|end| end.as_secs_f64()),
            "now": live_at.map(|at| at.as_secs_f64()),
        })
    });
    let ended = progress
        .as_ref()
        .map(|progress| progress.end.unwrap_or(progress.last));
    let failure = held.ending.as_ref().map(|ending| {
        let ended = ended.map(|ended| ended.as_secs_f64());
        failure::ended_json(ending, ended, &held.failures)
    });
    let lanes = progress
        .as_ref()
        .map(|progress| with_failures(lanes(progress, held.talk.as_ref()), &held.failures));
    let unread =
        progress::unread(unread).map(|said| json!({ "count": unread.len(), "said": said }));
    let mut run = json!({
        "id": id,
        "task": held.task,
        "live": held.ending.is_none(),
        "state": state,
        "has_result": row.has_result(),
        "verdict": verdict,
        "stage": phase,
        "result": result_json(held.ending.as_ref()),
        "failure": failure,
        "failures": held.failures.json(),
        "other_build": record.other_build(),
        "times": times,
        "lanes": lanes,
        "talk": held.talk.as_ref().map(Talk::json),
        "model": progress.as_ref().map(|progress| model_json(&progress.model)),
        "cost": progress.as_ref().and_then(Progress::cost).as_ref().map(cost_json),
        "driver": driver_json(record),
        "summary": summary,
        "unread": unread,
        "end": held.fold.taken(),
        "log_epoch": held.log_epoch,
    });
    if again {
        run["again"] = match controller::here(record, None, None) {
            Ok(args) => json!({ "command": next::run_again(&args) }),
            Err(error) => json!({ "error": error }),
        };
    }
    run
}

fn cost_json(cost: &Cost) -> Value {
    json!({
        "shown": progress::usd(cost.usd(), cost.partial()),
        "said": progress::cost::line(cost),
    })
}

/// What the run's driver used, in the words of the summary's `driver:` line. `shown` is the
/// driver's CPU time, and `rows` each figure by its name, the job's after the driver's. It is
/// `null` while the run is live, if its record keeps none, or if the record can't be read, which
/// the summary says.
fn driver_json(record: &Record) -> Value {
    let Ok(Some(used)) = record.driver_use() else {
        return Value::Null;
    };
    let (driver, job) = progress::driver::figures(&used);
    let job = (job.into_iter().flatten()).map(|(name, value)| (format!("job's {name}"), value));
    let rows: Vec<Value> = (driver.into_iter().chain(job))
        .map(|(name, value)| json!([name, value]))
        .collect();
    json!({ "shown": progress::driver::cpu(&used), "rows": rows })
}

fn model_json(model: &ModelUse) -> Value {
    let ModelUse {
        requests,
        answers: _,
        failed,
        not_2xx,
        last_good: _,
        in_flight,
        took,
        first_tokens,
        tokens,
        by_model: _,
        unreported: _,
        dropped_headers,
        more_dropped_headers,
    } = model;
    let failed: Vec<Value> = (failed.iter())
        .map(|(code, count)| json!({ "code": code, "count": count }))
        .collect();
    let in_flight: Vec<Value> = (in_flight.iter())
        .map(|(request, since)| json!({ "request": request, "since": since.as_secs_f64() }))
        .collect();
    let median = |spans: &[Duration]| progress::median(spans).map(|span| span.as_secs_f64());
    json!({
        "requests": requests,
        "failed": failed,
        "not_2xx": not_2xx,
        "in_flight": in_flight,
        "took": { "median": median(took), "count": took.len() },
        "first_token": { "median": median(first_tokens), "count": first_tokens.len() },
        "tokens": tokens_json(tokens, *requests),
        "dropped_headers": dropped_headers,
        "more_dropped_headers": more_dropped_headers,
    })
}

/// A lane's sandbox. It has a `start` only if the log timed the sandbox's start.
fn sandbox(sandbox: &runtime::Sandbox) -> Value {
    let mut json = json!({
        "place": progress::placed(sandbox),
        "asked": sandbox.asked.as_secs_f64(),
        "started": sandbox.started.map(|at| at.as_secs_f64()),
        "gone": sandbox.gone.map(|at| at.as_secs_f64()),
        "deleted": sandbox.deleted,
    });
    if let Some(start) = &sandbox.start {
        json["start"] = start_json(start);
    }
    json
}

/// A sandbox's start, each time in seconds since the run started. It gives each phase from its
/// start to its end, at most [`START_PHASES`], and how many more there are as `more_phases`.
fn start_json(start: &SandboxStart) -> Value {
    let SandboxStart {
        began,
        created,
        met,
        phases,
    } = start;
    let more_phases = phases.len().saturating_sub(START_PHASES);
    let phases: Vec<Value> = (phases.iter().take(START_PHASES))
        .map(|StartPhase { name, began, took }| {
            json!({
                "name": cut_text(name.clone()),
                "from": began.as_secs_f64(),
                "to": began.saturating_add(*took).as_secs_f64(),
            })
        })
        .collect();
    json!({
        "began": began.as_secs_f64(),
        "created": created.as_secs_f64(),
        "met": met.map(|at| at.as_secs_f64()),
        "phases": phases,
        "more_phases": more_phases,
    })
}

fn shared_lanes(progress: &Progress) -> impl Iterator<Item = Value> {
    progress.shared.iter().map(|(index, service)| {
        json!({
            "lane": format!("shared {index}"), "kind": "target", "lifetime": "work", "role": service.role,
            "sandbox": sandbox(&service.sandbox), "said": progress::work_target_line(service),
        })
    })
}

fn lanes(progress: &Progress, talk: Option<&Talk>) -> Vec<Value> {
    let now = progress.end.unwrap_or(progress.last);
    let span = now.as_secs_f64().max(1.0) / f64::from(LANE_TICKS);
    let answered = |caller: CallerName, json: &mut Value| {
        let answered = talk.and_then(|talk| talk.answered.get(&caller));
        let failed = talk.and_then(|talk| talk.failed.get(&caller));
        let (answered, failed) = (
            answered.map_or(&[][..], Vec::as_slice),
            failed.map_or(&[][..], Vec::as_slice),
        );
        json["answers"] = json!({ "count": answered.len(), "failed": failed.len() });
        let (answered, failed) = ticks(answered, failed, span);
        json["answered"] = json!(answered);
        json["failed"] = json!(failed);
    };
    let agents = progress.agents.iter().map(|(&caller, agent)| {
        let mut lane = json!({
            "lane": progress::caller_lane(caller),
            "caller": caller,
            "kind": "agent",
            "sandbox": sandbox(&agent.sandbox),
            "role": agent.role,
            "said": progress::agent_line(agent),
            "model": agent.model.as_ref().map(model_json),
        });
        answered(caller, &mut lane);
        lane
    });
    let shared = shared_lanes(progress);
    let calls = (progress.calls.iter()).map(|call| {
        let caller = CallerName::Job(call.job);
        let mut lane_json = json!({
            "lane": format!("job {}", call.job),
            "caller": caller,
            "kind": "target",
            "role": call.role,
            "of": call.of,
            "sandbox": sandbox(&call.sandbox),
            "said": progress::called(call),
            "model": model_json(&call.model),
        });
        if let Some(shared) = call.shared {
            lane_json["service"] = json!(format!("shared {shared}"));
        }
        if let Some(holder) = call.caller {
            lane_json["holder"] = json!(progress::caller_lane(holder));
        }
        answered(caller, &mut lane_json);
        ((call.sandbox.asked, call.sandbox.asked), lane_json)
    });
    let leases = (progress.leases.iter()).map(|lease| {
        let lane = json!({
            "lane": format!("lease {}", lease.lease),
            "kind": "runner",
            "role": talk.and_then(|talk| talk.runners.get(&lease.lease)),
            "grant": lease.grant,
            "holder": progress::caller_lane(lease.holder),
            "sandbox": sandbox(&lease.sandbox),
            "said": progress::leased(lease, now),
            "work": talk.map(|talk| talk.work_json(lease.lease)),
        });
        (progress::sorted(progress, lease), lane)
    });
    let mut children: Vec<((Duration, Duration), Value)> = calls.chain(leases).collect();
    children.sort_by_key(|(begun, _)| *begun);
    agents
        .chain(shared)
        .chain(children.into_iter().map(|(_, lane)| lane))
        .collect()
}

/// `lanes`, each that failed with the failure that `failures` holds of it.
fn with_failures(mut lanes: Vec<Value>, failures: &Failures) -> Vec<Value> {
    let failed = failures.of_lanes();
    for lane in &mut lanes {
        if let Some(failure) = lane["lane"].as_str().and_then(|name| failed.get(name)) {
            lane["failure"] = failure.clone();
        }
    }
    lanes
}

/// The marks of a lane's `answered` and `failed` times: one per span of the run's time, at the
/// span's first failure if it has one, else its first answer.
fn ticks(answered: &[f64], failed: &[f64], span: f64) -> (Vec<f64>, Vec<f64>) {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the span's index, clamped below the count of spans"
    )]
    let index = |time: f64| ((time / span).floor().max(0.0) as u32).min(LANE_TICKS - 1);
    let mut spans: BTreeMap<u32, (f64, Option<f64>)> = BTreeMap::new();
    for &time in answered {
        let first = spans.entry(index(time)).or_insert((time, None));
        first.0 = first.0.min(time);
    }
    for &time in failed {
        let first = spans.entry(index(time)).or_insert((time, None));
        first.1 = Some(first.1.map_or(time, |first| time.min(first)));
    }
    let ticks = spans
        .values()
        .map(|(answered, failed)| failed.unwrap_or(*answered));
    let failed = spans.values().filter_map(|(_, failed)| *failed);
    (ticks.collect(), failed.collect())
}
