//! The JSON forms of what the view answers with: a run, the list's row of one, and the records
//! of its log, each said as `ls` says it where `ls` says it ([`progress`]). Which request each
//! answers, and what the view holds to make it from, is the parent module's.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use runtime::{Ending, ModelUse, Progress, Readout, Tokens};
use serde_json::{Value, json};

use super::talk::Talk;
use super::{Held, when};
use crate::progress::{self, Listed};
use crate::{controller, next};

/// The most bytes of a string of a log's record that an answer holds; it says how long the string
/// is in all.
const STRING_CUT_BYTES: usize = 8 * 1024;

/// The most times of model answers that a lane holds, each the start of one of as many equal
/// spans of the run so far that holds an answer: a lane's track on a page is about a thousand
/// pixels wide, and a run makes thousands of requests.
const LANE_TICKS: u32 = 500;

/// The record that `line` of a log holds, which starts at `byte`, as an answer holds it: with
/// `"byte"`, and `"when"`, its time since the run started in seconds, its strings cut
/// ([`cut`]); or `{"byte": b, "unread": text}` if it is no JSON object.
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

/// `value` with each string longer than [`STRING_CUT_BYTES`] cut there, at a character's start,
/// and the bytes it holds in all said after it.
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

/// The list's row of `held`, a live run, as JSON.
pub(super) fn live_row(held: &Held, named: Option<&str>) -> Value {
    let progress = held.readout.progress.clone();
    let model = progress.as_ref().map(|progress| progress.model.clone());
    let listed = Listed::of(&held.record, held.task.clone(), None, progress);
    let mut row = listed_json(held.record.id(), &listed, false, crate::unix_now(), named);
    row["requests"] = json!(model.as_ref().map(|model| model.requests));
    row["tokens"] = json!(
        model
            .as_ref()
            .map(|model| tokens_json(&model.tokens, model.requests))
    );
    row
}

/// Whether a run that ended as `ending` says, if it has, finished with a score.
pub(super) fn scored(ending: Option<&Ending>) -> bool {
    matches!(ending, Some(Ending::Scored { .. }))
}

/// Run `id`, `listed`, as `ls` lists it at `now`, as JSON; `scored` is whether it finished with a
/// score, which its state doesn't say.
pub(super) fn listed_json(
    id: &str,
    listed: &Listed,
    scored: bool,
    now: provider::UnixTime,
    named: Option<&str>,
) -> Value {
    let progress::Columns {
        state,
        time,
        bound,
        usd,
        rest,
    } = progress::columns(id, listed, now, named);
    json!({
        "id": id,
        "live": listed.live(),
        "state": state,
        "scored": scored,
        "time": time,
        "bound": bound,
        "usd": usd,
        "rest": rest,
    })
}

/// The `tokens` that `requests` model requests reported, as JSON: `{"input", "output",
/// "cached"}`, each `null` that no response gave, and `input` and `output` too where requests
/// were made and no response reported its tokens, which is not none used.
pub(super) fn tokens_json(tokens: &Tokens, requests: u64) -> Value {
    let Tokens {
        reported,
        input,
        output,
        cached,
        reasoning: _,
    } = tokens;
    let known = *reported > 0 || requests == 0;
    json!({
        "input": known.then_some(input),
        "output": known.then_some(output),
        "cached": cached,
    })
}

/// How a run that ended as `ending` says ended, in words that follow its id.
fn how(ending: &Ending) -> String {
    match ending {
        Ending::Scored { score, after } => format!(
            "finished with a score of {}, {after}",
            progress::shown_score(score)
        ),
        Ending::Unscored(why) | Ending::NoResult { why, .. } => why.clone(),
        Ending::Failed(how) | Ending::Interrupted(how) => how.clone(),
        Ending::Lost => "is lost: its driver ended without writing outcome.json".to_owned(),
    }
}

/// The run of `held`, as JSON, as of its last look: `{"id", "task", "live", "state", "scored",
/// "verdict", "again", "other_build", "times", "lanes", "talk", "model", "summary", "unread",
/// "end", "log_epoch"}`, where `again` is `{"command": c}`, the command that runs it again from
/// here, or `{"error": e}`, why none can be made; its runs directory is `named`, as commands name
/// it, if not by default.
pub(super) fn run_json(held: &Held, named: Option<&str>) -> Value {
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
            } = progress::columns(id, &listed, now, named);
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
        (Some(_), _) | (None, Some(_)) => listed.state().name(),
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
    json!({
        "id": id,
        "task": held.task,
        "live": held.ending.is_none(),
        "state": state,
        "scored": scored(held.ending.as_ref()),
        "verdict": verdict,
        "again": match controller::here(record, None) {
            Ok(args) => json!({ "command": next::run_again(&args) }),
            Err(error) => json!({ "error": error }),
        },
        "other_build": record.other_build(),
        "times": times,
        "lanes": progress.as_ref().map(|progress| lanes(record.dir(), progress, held.talk.as_ref())),
        "talk": held.talk.as_ref().map(Talk::json),
        "model": progress.as_ref().map(|progress| model_json(&progress.model)),
        "summary": summary,
        "unread": unread,
        "end": held.fold.taken(),
        "log_epoch": held.log_epoch,
    })
}

/// `model`, a run's or a judge call's model requests, as JSON: how many, how many stated a
/// failure, by code, how many were not forwarded whole with a 2xx status, those in flight with
/// when each went, the medians of how long the good ones took to their end and to their first
/// token, with how many each is of, the tokens reported, and the names of the request headers
/// that the route left behind, the first [`route::MAX_DROPPED_HEADERS`] by name, with whether it
/// left more.
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

/// The lanes of the run's sandboxes, of the record in `dir`, as its summary orders them: the
/// agent's, then each judge call's and lease's, by when the core began it, a lease under the call
/// that holds it; each with what the summary says of it, and each caller's with its `caller`,
/// as a trace names it (`agent`, `job-<n>`), and its model answers: `"answers": {"count",
/// "failed"}`, how many, and `"answered"` and `"failed"`, a time of each of [`LANE_TICKS`] spans
/// of the run so far that holds one ([`ticks`]).
fn lanes(dir: &Path, progress: &Progress, talk: Option<&Talk>) -> Vec<Value> {
    let now = progress.end.unwrap_or(progress.last);
    let span = now.as_secs_f64().max(1.0) / f64::from(LANE_TICKS);
    let sandbox = |sandbox: &runtime::Sandbox| {
        json!({
            "place": progress::placed(sandbox),
            "asked": sandbox.asked.as_secs_f64(),
            "started": sandbox.started.map(|at| at.as_secs_f64()),
            "gone": sandbox.gone.map(|at| at.as_secs_f64()),
            "deleted": sandbox.deleted,
        })
    };
    let answered = |lane: &str, json: &mut Value| {
        let answered = talk.and_then(|talk| talk.answered.get(lane));
        let failed = talk.and_then(|talk| talk.failed.get(lane));
        let (answered, failed) = (
            answered.map_or(&[][..], Vec::as_slice),
            failed.map_or(&[][..], Vec::as_slice),
        );
        json["answers"] = json!({ "count": answered.len(), "failed": failed.len() });
        let (answered, failed) = ticks(answered, failed, span);
        json["answered"] = json!(answered);
        json["failed"] = json!(failed);
    };
    let mut agent = json!({
        "lane": "agent",
        "caller": "agent",
        "kind": "agent",
        "sandbox": sandbox(&progress.agent.sandbox),
        "said": progress::agent_line(progress),
    });
    answered("agent", &mut agent);
    let calls = (progress.calls.iter()).map(|call| {
        let lane = format!("job {}", call.job);
        let mut lane_json = json!({
            "lane": lane,
            "caller": format!("job-{}", call.job),
            "kind": "judge",
            "role": call.role,
            "grant": call.grant,
            "sandbox": sandbox(&call.sandbox),
            "said": progress::called(call, dir),
            "model": model_json(&call.model),
        });
        answered(&lane, &mut lane_json);
        ((call.sandbox.asked, call.sandbox.asked), lane_json)
    });
    let leases = (progress.leases.iter()).map(|lease| {
        let lane = json!({
            "lane": format!("lease {}", lease.lease),
            "kind": "runner",
            "role": talk.and_then(|talk| talk.runners.get(&lease.lease)),
            "grant": lease.grant,
            "holder": lease.holder.map_or("agent".to_owned(), |job| format!("job {job}")),
            "sandbox": sandbox(&lease.sandbox),
            "said": progress::leased(lease, now),
            "work": talk.map(|talk| talk.work_json(lease.lease)),
        });
        (progress::sorted(progress, lease), lane)
    });
    let mut children: Vec<((Duration, Duration), Value)> = calls.chain(leases).collect();
    children.sort_by_key(|(begun, _)| *begun);
    std::iter::once(agent)
        .chain(children.into_iter().map(|(_, lane)| lane))
        .collect()
}

/// Of `answered`, times in seconds since the run's start, one in each of the spans `span` seconds
/// long from the run's start that holds one, at most [`LANE_TICKS`], as a time past the last span
/// is put in it: the first of `failed`, a part of `answered`, that the span holds, else its first;
/// and those of `failed`.
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
