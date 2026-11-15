//! A trace's statistics: each step's tokens, cost, times and calls, and their folds by turn, by
//! thread and over the caller, from the held pass alone and the prices that the run's record keeps.
//!
//! An unknown value stays unknown: a fold sums the values its steps know and says how many of
//! its steps lacked one, and a rate or share is the fold's sums divided, never an average of its
//! steps' own. A time is a span between two of the log's times, in nanoseconds.

use std::collections::{BTreeMap, BTreeSet};

use route::read::Item;
use runtime::{TokenPrices, Tokens};
use serde_json::{Map, Value, json};

use super::{Exchanges, Trace, nanos};

/// The shortest decode whose rate of output tokens is said: over a shorter one, the network's
/// timing outweighs the model's.
const RATE_DECODE_NS: u64 = 100_000_000;

/// The tokens a step's usage reports, each `<name>_tokens` in the log.
const TOKENS: [&str; 5] = ["input", "cached", "cache_write", "output", "reasoning"];

/// What a fold by turn or thread groups.
#[derive(Clone, Copy)]
pub(in crate::view) enum By {
    Step,
    Turn,
    Thread,
}

/// How a step's request ended.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Outcome {
    /// Answered with a success.
    Ok,
    /// Answered by its upstream with a failure, or cut short.
    Failed,
    /// Refused or barred by Pumpkin.
    Refused,
    #[default]
    InFlight,
}

/// A step's measures, each `None` if unknown.
#[derive(Default)]
struct Measured {
    request: u64,
    thread: Option<u64>,
    turn: Option<u64>,
    /// In the order of [`TOKENS`].
    tokens: [Option<u64>; 5],
    usd: Option<f64>,
    start: Option<u64>,
    end: Option<u64>,
    /// From its parent's end to its start: the tools' and the harness's time.
    queue: Option<u64>,
    to_first_byte: Option<u64>,
    to_first_token: Option<u64>,
    /// From its first token to its end.
    decode: Option<u64>,
    /// From its start to its end.
    model: Option<u64>,
    calls: Vec<String>,
    status: Option<u64>,
    outcome: Outcome,
    retry_of: Option<u64>,
    repeat_of: Option<u64>,
}

/// The span from `from` to `to`, if both are known and in that order.
pub(super) fn span(from: Option<u64>, to: Option<u64>) -> Option<u64> {
    to?.checked_sub(from?)
}

impl Trace {
    /// The statistics of the trace's steps, as rows of columns, a step's each or a fold's of
    /// `by`'s groups, and the fold of them all; each model's tokens cost `prices`, by the route's
    /// name for it.
    pub(super) fn stats_json(
        &self,
        exchanges: &Exchanges,
        prices: &BTreeMap<String, TokenPrices>,
        by: By,
    ) -> Value {
        let measured = self.measured(exchanges, prices);
        let rows: Vec<Vec<(String, Value)>> = match by {
            By::Step => measured.iter().map(step_row).collect(),
            By::Turn | By::Thread => {
                let mut groups: BTreeMap<Option<u64>, Vec<&Measured>> = BTreeMap::new();
                for step in &measured {
                    let key = if matches!(by, By::Turn) {
                        step.turn
                    } else {
                        step.thread
                    };
                    groups.entry(key).or_default().push(step);
                }
                (groups.into_iter())
                    .map(|(key, steps)| group_row(by, key, &steps))
                    .collect()
            }
        };
        let names: Vec<String> = match by {
            By::Step => step_row(&Measured::default()),
            By::Turn | By::Thread => group_row(by, None, &[]),
        }
        .into_iter()
        .map(|(name, _)| name)
        .collect();
        let mut columns: Vec<Vec<Value>> = vec![Vec::new(); names.len()];
        for row in rows {
            for (column, (_, value)) in columns.iter_mut().zip(row) {
                column.push(value);
            }
        }
        let rows: Map<String, Value> = (names.into_iter())
            .zip(columns.into_iter().map(Value::Array))
            .collect();
        let all: Vec<&Measured> = measured.iter().collect();
        let mut totals: Map<String, Value> = fold(&all).into_iter().collect();
        let count = |key: fn(&Measured) -> Option<u64>| {
            let keys: BTreeSet<u64> = measured.iter().filter_map(key).collect();
            json!(keys.len())
        };
        totals.insert("threads".to_owned(), count(|step| step.thread));
        totals.insert("turns".to_owned(), count(|step| step.turn));
        let by = match by {
            By::Step => "step",
            By::Turn => "turn",
            By::Thread => "thread",
        };
        json!({ "caller": self.caller, "by": by, "rows": rows, "totals": totals })
    }

    /// Each step's measures, in the order of their requests.
    fn measured(
        &self,
        exchanges: &Exchanges,
        prices: &BTreeMap<String, TokenPrices>,
    ) -> Vec<Measured> {
        (self.at.values())
            .filter_map(|&index| self.measure(exchanges, prices, index))
            .collect()
    }

    /// The measures of step `index`, if the log names its request.
    fn measure(
        &self,
        exchanges: &Exchanges,
        prices: &BTreeMap<String, TokenPrices>,
        index: usize,
    ) -> Option<Measured> {
        let ended = |index: usize| {
            let said = exchanges.by_request.get(&self.steps[index].request)?;
            said.answered.as_ref().and_then(|(_, ns, _)| *ns)
        };
        let step = &self.steps[index];
        let said = exchanges.by_request.get(&step.request)?;
        let event = said.event();
        let start = said.started.and_then(|(_, ns)| ns);
        let end = ended(index);
        let first_byte = nanos(&event["first_byte_ns"]);
        let first_token = nanos(&event["first_token_ns"]);
        let usage = &event["usage"];
        let tokens = TOKENS.map(|name| usage[format!("{name}_tokens")].as_u64());
        let usd = (event["model"].as_str())
            .and_then(|model| prices.get(model))
            .and_then(|prices| {
                let [input, cached, cache_write, output, reasoning] = tokens;
                prices.usd(&Tokens {
                    reported: 1,
                    input: input?,
                    output: output?,
                    cached,
                    cache_write,
                    reasoning,
                })
            });
        let (retry_of, repeat_of) = self.same(exchanges, step);
        let refused = matches!(event["answer"]["kind"].as_str(), Some("refused" | "barred"));
        let outcome = match self.answered_ok(exchanges, index) {
            None => Outcome::InFlight,
            Some(true) => Outcome::Ok,
            Some(false) if refused => Outcome::Refused,
            Some(false) => Outcome::Failed,
        };
        let calls = (step.answer.iter())
            .filter_map(|item| match item {
                Item::Call { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        Some(Measured {
            request: step.request,
            thread: step.thread,
            turn: step.turn,
            tokens,
            usd,
            start,
            end,
            queue: span(step.parent.and_then(ended), start),
            to_first_byte: span(start, first_byte),
            to_first_token: span(start, first_token),
            decode: span(first_token, end),
            model: span(start, end),
            calls,
            status: event["upstream_status"].as_u64(),
            outcome,
            retry_of,
            repeat_of,
        })
    }

    /// The statistics of step `index`, as the `stats` reply's row of it gives them, as an object.
    pub(super) fn step_stats(
        &self,
        exchanges: &Exchanges,
        prices: &BTreeMap<String, TokenPrices>,
        index: usize,
    ) -> Value {
        self.measure(exchanges, prices, index)
            .map_or(Value::Null, |step| {
                Value::Object(step_row(&step).into_iter().collect())
            })
    }

    /// The span of each of `threads`, as the `stats` reply's fold by thread gives it, from its
    /// steps' first start to their last end, as columns `thread` and `span_ns`.
    pub(super) fn thread_spans(&self, exchanges: &Exchanges, threads: &BTreeSet<u64>) -> Value {
        let mut bounds: BTreeMap<u64, (Option<u64>, Option<u64>)> = threads
            .iter()
            .map(|&thread| (thread, (None, None)))
            .collect();
        for step in &self.steps {
            let Some((first, last)) = step.thread.and_then(|thread| bounds.get_mut(&thread)) else {
                continue;
            };
            let Some(said) = exchanges.by_request.get(&step.request) else {
                continue;
            };
            let start = said.started.and_then(|(_, ns)| ns);
            let end = said.answered.as_ref().and_then(|(_, ns, _)| *ns);
            *first = (*first).into_iter().chain(start).min();
            *last = (*last).max(end);
        }
        let (threads, spans): (Vec<u64>, Vec<Option<u64>>) = (bounds.into_iter())
            .map(|(thread, (first, last))| (thread, span(first, last)))
            .unzip();
        json!({ "thread": threads, "span_ns": spans })
    }
}

/// The output tokens a second over `decode`, if it is long enough to say ([`RATE_DECODE_NS`]).
#[expect(
    clippy::cast_precision_loss,
    reason = "a rate: tokens and nanoseconds past 2^52 are past any run"
)]
fn rate(output: u64, decode: u64) -> Option<f64> {
    (decode >= RATE_DECODE_NS).then(|| output as f64 * 1e9 / decode as f64)
}

/// `part` as a fraction of `whole`, if `whole` isn't 0.
#[expect(
    clippy::cast_precision_loss,
    reason = "a share: tokens past 2^52 are past any run"
)]
pub(super) fn share(part: u64, whole: u64) -> Option<f64> {
    (whole > 0).then(|| part as f64 / whole as f64)
}

/// The count of each name among `calls`.
fn by_name<'c>(calls: impl Iterator<Item = &'c String>) -> Value {
    let mut counts: BTreeMap<&str, u64> = BTreeMap::new();
    for call in calls {
        *counts.entry(call).or_default() += 1;
    }
    json!(counts)
}

/// The row of `step`'s measures.
fn step_row(step: &Measured) -> Vec<(String, Value)> {
    let [input, cached, ..] = step.tokens;
    let output = step.tokens[3];
    let outcome = match step.outcome {
        Outcome::Ok => "ok",
        Outcome::Failed => "failed",
        Outcome::Refused => "refused",
        Outcome::InFlight => "in_flight",
    };
    let mut row = vec![
        ("request".to_owned(), json!(step.request)),
        ("thread".to_owned(), json!(step.thread)),
        ("turn".to_owned(), json!(step.turn)),
    ];
    row.extend(
        (TOKENS.iter())
            .zip(step.tokens)
            .map(|(name, tokens)| ((*name).to_owned(), json!(tokens))),
    );
    row.push(("usd".to_owned(), json!(step.usd)));
    let tok_s = output
        .zip(step.decode)
        .and_then(|(output, decode)| rate(output, decode));
    row.extend([
        (
            "cache_hit".to_owned(),
            json!(cached.zip(input).and_then(|(c, i)| share(c, i))),
        ),
        ("queue_ns".to_owned(), json!(step.queue)),
        ("to_first_byte_ns".to_owned(), json!(step.to_first_byte)),
        ("to_first_token_ns".to_owned(), json!(step.to_first_token)),
        ("decode_ns".to_owned(), json!(step.decode)),
        ("model_ns".to_owned(), json!(step.model)),
        ("tok_s".to_owned(), json!(tok_s)),
        ("calls".to_owned(), by_name(step.calls.iter())),
        ("status".to_owned(), json!(step.status)),
        ("outcome".to_owned(), json!(outcome)),
        ("retry_of".to_owned(), json!(step.retry_of)),
        ("repeat_of".to_owned(), json!(step.repeat_of)),
    ]);
    row
}

/// The row of the group of `steps` whose turn or thread, as `by` groups, is `key`: its key, its
/// thread, and their fold.
fn group_row(by: By, key: Option<u64>, steps: &[&Measured]) -> Vec<(String, Value)> {
    let mut row = vec![];
    if matches!(by, By::Turn) {
        row.push(("turn".to_owned(), json!(key)));
    }
    let thread = steps.first().and_then(|step| step.thread);
    row.push(("thread".to_owned(), json!(thread)));
    row.extend(fold(steps));
    row
}

/// The sum of the known among `values`, `null` if none is, and the count of those unknown.
fn sum(values: impl Iterator<Item = Option<u64>>) -> (Value, usize) {
    let (mut sum, mut known, mut unknown) = (0_u64, false, 0);
    for value in values {
        match value {
            Some(value) => (sum, known) = (sum.saturating_add(value), true),
            None => unknown += 1,
        }
    }
    (if known { json!(sum) } else { Value::Null }, unknown)
}

/// The sum of the known among `values`, in dollars, `null` if none is, and the count of those
/// unknown.
fn sum_usd(values: impl Iterator<Item = Option<f64>>) -> (Value, usize) {
    let (mut sum, mut known, mut unknown) = (0.0, false, 0);
    for value in values {
        match value {
            Some(value) => (sum, known) = (sum + value, true),
            None => unknown += 1,
        }
    }
    (if known { json!(sum) } else { Value::Null }, unknown)
}

/// The median of the known among `values`, the lower of the middle two of an even count, and
/// the count of those unknown.
fn median(values: impl Iterator<Item = Option<u64>>) -> (Value, usize) {
    let values: Vec<Option<u64>> = values.collect();
    let mut known: Vec<u64> = values.iter().flatten().copied().collect();
    known.sort_unstable();
    let middle = known.len().checked_sub(1).map(|last| known[last / 2]);
    (json!(middle), values.len() - known.len())
}

/// The fold of `steps`: their count and first request, their span from first start to last end,
/// their sums of tokens, cost, and the queue, decode and model times, the medians of their times to
/// a first byte and token, each with the count that lacked it; the cache's share of the input
/// and the rate of output, of the steps that know both; their calls by name, and how many were
/// retries, repeats, refused, failed and in flight.
#[expect(clippy::too_many_lines, reason = "a column of the fold after another")]
fn fold(steps: &[&Measured]) -> Vec<(String, Value)> {
    let mut row = vec![
        ("steps".to_owned(), json!(steps.len())),
        (
            "first_request".to_owned(),
            json!(steps.first().map(|step| step.request)),
        ),
        (
            "span_ns".to_owned(),
            json!(span(
                steps.iter().filter_map(|step| step.start).min(),
                steps.iter().filter_map(|step| step.end).max()
            )),
        ),
    ];
    let mut with_unknown = |name: &str, (value, unknown): (Value, usize)| {
        row.push((name.to_owned(), value));
        row.push((format!("{name}_unknown"), json!(unknown)));
    };
    for (at, name) in TOKENS.iter().enumerate() {
        with_unknown(name, sum(steps.iter().map(|step| step.tokens[at])));
    }
    with_unknown("usd", sum_usd(steps.iter().map(|step| step.usd)));
    with_unknown("queue_ns", sum(steps.iter().map(|step| step.queue)));
    with_unknown("decode_ns", sum(steps.iter().map(|step| step.decode)));
    with_unknown("model_ns", sum(steps.iter().map(|step| step.model)));
    with_unknown(
        "median_to_first_byte_ns",
        median(steps.iter().map(|step| step.to_first_byte)),
    );
    with_unknown(
        "median_to_first_token_ns",
        median(steps.iter().map(|step| step.to_first_token)),
    );
    let (mut cached, mut input) = (0, 0);
    for step in steps {
        if let [Some(of), Some(hit), ..] = step.tokens {
            (cached, input) = (cached + hit, input + of);
        }
    }
    let (mut output, mut decode) = (0, 0);
    for step in steps {
        if let (Some(tokens), Some(ns)) = (step.tokens[3], step.decode)
            && ns >= RATE_DECODE_NS
        {
            (output, decode) = (output + tokens, decode + ns);
        }
    }
    let count = |is: fn(&Measured) -> bool| json!(steps.iter().filter(|step| is(step)).count());
    row.extend([
        ("cache_hit".to_owned(), json!(share(cached, input))),
        ("tok_s".to_owned(), json!(rate(output, decode))),
        (
            "calls".to_owned(),
            by_name(steps.iter().flat_map(|step| &step.calls)),
        ),
        ("retries".to_owned(), count(|step| step.retry_of.is_some())),
        ("repeats".to_owned(), count(|step| step.repeat_of.is_some())),
        (
            "refusals".to_owned(),
            count(|step| step.outcome == Outcome::Refused),
        ),
        (
            "failures".to_owned(),
            count(|step| step.outcome == Outcome::Failed),
        ),
        (
            "in_flight".to_owned(),
            count(|step| step.outcome == Outcome::InFlight),
        ),
    ]);
    row
}
