//! The JSON that the view answers with for a run and its log: the forms of what `runtime` reads
//! from a record and of what [`super::talk`] folds from its log.

use std::collections::BTreeMap;
use std::time::Duration;

use runtime::{CallerName, Cost, Ending, ModelUse, Progress, Readout, Tokens};
use serde_json::{Value, json};

use super::talk::Talk;
use super::{Held, when};
use crate::progress::{self, Listed, Row};
use crate::{controller, next};

/// The longest string a reply carries whole; a longer one is cut, and its entry is read apart.
const STRING_CUT_BYTES: usize = 8 * 1024;

/// How many marks at most a lane's answers and failures are drawn as across the run's time: a
/// lane's track on a page is about a thousand pixels wide, and a run makes thousands of requests.
const LANE_TICKS: u32 = 500;

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

/// `value` with each string longer than [`STRING_CUT_BYTES`] cut, at a character, and marked with
/// its length in all, which [`was_cut`] reads.
pub(super) fn cut(value: Value) -> Value {
    match value {
        Value::String(text) if text.len() > STRING_CUT_BYTES => {
            let at = text.floor_char_boundary(STRING_CUT_BYTES);
            Value::String(format!("{}… [{} bytes in all]", &text[..at], text.len()))
        }
        Value::Array(values) => Value::Array(values.into_iter().map(cut).collect()),
        Value::Object(fields) => Value::Object(
            (fields.into_iter())
                .map(|(name, value)| (name, cut(value)))
                .collect(),
        ),
        Value::String(_) | Value::Null | Value::Bool(_) | Value::Number(_) => value,
    }
}

/// Whether `text` ends with the mark that [`cut`] puts on a string.
pub(super) fn was_cut(text: &str) -> bool {
    let Some(rest) = text.strip_suffix(" bytes in all]") else {
        return false;
    };
    let length = rest.rsplit_once("… [").map(|(_, length)| length);
    length.is_some_and(|length| length.parse::<usize>().is_ok())
}

/// The row of the run list for a run that has not ended.
pub(super) fn live_row(held: &Held, named: Option<&str>) -> Value {
    let progress = held.readout.progress.clone();
    let model = progress.as_ref().map(|progress| progress.model.clone());
    let listed = Listed::of(&held.record, held.task.clone(), None, progress);
    let id = held.record.id();
    let mut row = listed_json(id, &Row::of(id, &listed, crate::unix_now(), named));
    row["requests"] = json!(model.as_ref().map(|model| model.requests));
    row["tokens"] = json!(
        model
            .as_ref()
            .map(|model| tokens_json(&model.tokens, model.requests))
    );
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
pub(super) fn run_json(held: &Held, named: Option<&str>, again: bool) -> Value {
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
    let row = Row::of(id, &listed, now, named);
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
    let unread =
        progress::unread(unread).map(|said| json!({ "count": unread.len(), "said": said }));
    let mut run = json!({
        "id": id,
        "task": held.task,
        "live": held.ending.is_none(),
        "state": state,
        "has_result": row.has_result(),
        "verdict": verdict,
        "result": result_json(held.ending.as_ref()),
        "other_build": record.other_build(),
        "times": times,
        "lanes": progress.as_ref().map(|progress| lanes(progress, held.talk.as_ref())),
        "talk": held.talk.as_ref().map(Talk::json),
        "model": progress.as_ref().map(|progress| model_json(&progress.model)),
        "cost": progress.as_ref().and_then(Progress::cost).as_ref().map(cost_json),
        "summary": summary,
        "unread": unread,
        "end": held.fold.taken(),
        "log_epoch": held.log_epoch,
    });
    if again {
        run["again"] = match controller::here(record, None) {
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

fn sandbox(sandbox: &runtime::Sandbox) -> Value {
    json!({
        "place": progress::placed(sandbox),
        "asked": sandbox.asked.as_secs_f64(),
        "started": sandbox.started.map(|at| at.as_secs_f64()),
        "gone": sandbox.gone.map(|at| at.as_secs_f64()),
        "deleted": sandbox.deleted,
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
        .chain(children.into_iter().map(|(_, lane)| lane))
        .collect()
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
