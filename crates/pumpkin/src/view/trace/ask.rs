//! What a request of a run's trace asks for, below `/api/runs/<id>/`, and the answer it gets:
//! which pass of the trace reads it, made or extended first, and the bytes of an exchange or an
//! item as the record keeps them. The trace is the parent module's, and the JSON forms of its
//! answers are [`json`](mod@super::json)'s.

use std::collections::BTreeMap;

use runtime::{CallerName, Record};
use schema::ir::Sha256Digest;
use serde::de::DeserializeOwned;
use serde::de::value::{Error, StrDeserializer};
use serde_json::{Value, json};

use super::super::http::{Request, Response};
use super::super::{Failed, Held, View, line_at};
use super::json::{EntryKind, exchange_json};
use super::{Address, Exchanges, TRACE_PASSES, Trace, diff, stats};

/// The most bytes of a search's text: each search reads every entry of the trace.
const SEARCH_QUERY_BYTES: usize = 1024;

/// What a request of a run's trace asks for, below `/api/runs/<id>/`.
pub(in crate::view) enum Asked {
    Steps {
        caller: CallerName,
        since: Option<u64>,
        log_epoch: Option<u64>,
    },
    Entries {
        caller: CallerName,
        from: u64,
        to: u64,
        entry: usize,
        kind: Option<EntryKind>,
        thread: Option<u64>,
    },
    Search {
        caller: CallerName,
        query: String,
        kind: Option<EntryKind>,
        thread: Option<u64>,
    },
    Entry {
        caller: CallerName,
        request: u64,
        address: Address,
    },
    Exchange {
        request: u64,
        part: Option<Side>,
    },
    Item {
        digest: Sha256Digest,
    },
    Payload {
        digest: Sha256Digest,
    },
    Stats {
        caller: CallerName,
        by: stats::By,
    },
    Ops {
        caller: CallerName,
    },
    Diff {
        a: u64,
        b: u64,
        part: diff::Compared,
    },
}

/// The bytes of an exchange that a request asks for.
#[derive(Clone, Copy)]
pub(in crate::view) enum Side {
    Request,
    Response,
}

/// What `parts`, the path's parts past the run's id, and `request` ask for; refused if nothing
/// is there, or a number, caller, kind or digest is none.
#[expect(clippy::too_many_lines, reason = "an arm for each of its paths")]
pub(in crate::view) fn asked(parts: &[&str], request: &Request) -> Result<Asked, Response> {
    let bad = |message: String| Response::refused("400 Bad Request", &message);
    let caller = |caller: &str| {
        // A caller has one name, as the log writes it, so `job-01` is none.
        let parsed = named::<CallerName>(caller)
            .ok()
            .filter(|parsed| parsed.to_string() == caller);
        parsed.ok_or_else(|| {
            bad(format!(
                "`{caller}` is no caller: expected `agent-<n>` or `job-<n>`"
            ))
        })
    };
    let kind = || {
        let Some(kind) = request.query("kind") else {
            return Ok(None);
        };
        // serde's message lists the kinds there are.
        let kind = named(kind).map_err(|error| bad(format!("`kind` is no entry's kind: {error}")));
        kind.map(Some)
    };
    let number = |name: &str, value: &str| {
        value
            .parse::<u64>()
            .map_err(|_| bad(format!("the {name} is `{value}`: expected a whole number")))
    };
    let request_of = |name: &str| {
        request
            .number(name)?
            .ok_or_else(|| bad(format!("give a request's number: `?{name}=<request>`")))
    };
    match parts {
        ["trace", who, "steps"] => Ok(Asked::Steps {
            caller: caller(who)?,
            since: request.number("since")?,
            log_epoch: request.number("log_epoch")?,
        }),
        ["trace", who, "entries"] => {
            let (caller, from, to) = (caller(who)?, request_of("from")?, request_of("to")?);
            if to < from {
                return Err(bad(format!(
                    "`to` is {to}, before `from`, {from}: expected `from` <= `to`"
                )));
            }
            Ok(Asked::Entries {
                caller,
                from,
                to,
                entry: (request.number("entry")?.unwrap_or(0))
                    .try_into()
                    .unwrap_or(usize::MAX),
                kind: kind()?,
                thread: request.number("thread")?,
            })
        }
        ["trace", who, "search"] => {
            let query = request.query("q").unwrap_or_default();
            if query.len() > SEARCH_QUERY_BYTES {
                return Err(bad(format!(
                    "`q` is {} bytes: expected at most {SEARCH_QUERY_BYTES}",
                    query.len()
                )));
            }
            Ok(Asked::Search {
                caller: caller(who)?,
                query: query.to_owned(),
                kind: kind()?,
                thread: request.number("thread")?,
            })
        }
        ["trace", who, "entry"] => {
            let caller = caller(who)?;
            let at = request
                .query("at")
                .ok_or_else(|| bad("give an entry's address: `?at=<address>`".to_owned()))?;
            let (request, address) = Address::parse(at).ok_or_else(|| {
                bad(format!(
                    "`{at}` is no entry's address: expected `<request>/in/<item>/<part>`, \
                     `<request>/head/<field>/<part>` or `<request>/out/<part>`"
                ))
            })?;
            Ok(Asked::Entry {
                caller,
                request,
                address,
            })
        }
        ["exchange", n] => Ok(Asked::Exchange {
            request: number("request", n)?,
            part: None,
        }),
        ["exchange", n, side @ ("request" | "response")] => Ok(Asked::Exchange {
            request: number("request", n)?,
            part: Some(if *side == "request" {
                Side::Request
            } else {
                Side::Response
            }),
        }),
        ["trace", who, "stats"] => Ok(Asked::Stats {
            caller: caller(who)?,
            by: match request.query("by").unwrap_or("step") {
                "step" => stats::By::Step,
                "turn" => stats::By::Turn,
                "thread" => stats::By::Thread,
                by => {
                    return Err(bad(format!(
                        "`by` is `{by}`: expected `step`, `turn` or `thread`"
                    )));
                }
            },
        }),
        ["trace", who, "ops"] => Ok(Asked::Ops {
            caller: caller(who)?,
        }),
        ["diff"] => Ok(Asked::Diff {
            a: request_of("a")?,
            b: request_of("b")?,
            part: match request.query("part") {
                Some("system") => diff::Compared::System,
                Some("tools") => diff::Compared::Tools,
                part => {
                    return Err(bad(format!(
                        "`part` is `{}`: expected `system` or `tools`",
                        part.unwrap_or_default()
                    )));
                }
            },
        }),
        ["item", hex] => Ok(Asked::Item {
            digest: Sha256Digest::try_from(format!("sha256:{hex}")).map_err(|_| {
                bad(format!(
                    "`{hex}` is no item: expected 64 lowercase hexadecimal digits"
                ))
            })?,
        }),
        ["payload", hex] => Ok(Asked::Payload {
            digest: Sha256Digest::try_from(format!("sha256:{hex}")).map_err(|_| {
                bad(format!(
                    "`{hex}` is no payload: expected 64 lowercase hexadecimal digits"
                ))
            })?,
        }),
        _ => Err(Response::refused(
            "404 Not Found",
            &format!(
                "nothing is at `{}`: expected a run's `trace/<caller>/steps`, `…/entries`, \
                 `…/entry`, `…/search`, `…/stats` or `…/ops`, `exchange/<n>`, `item/<hex>`, `payload/<hex>`, or `diff`",
                request.path
            ),
        )),
    }
}

/// `name` as a JSON string of it reads as a `T`, or why it names none.
fn named<T: DeserializeOwned>(name: &str) -> Result<T, Error> {
    T::deserialize(StrDeserializer::<Error>::new(name))
}

/// What a trace's request is answered with: JSON, or bytes as kept.
pub(in crate::view) enum Answer {
    Json(Value),
    Bytes(Response),
}

impl View {
    /// The answer to `asked`, of run `id`'s trace, or one of its exchanges or items.
    pub(in crate::view) fn trace(
        &mut self,
        id: &provider::RunTag,
        asked: Asked,
    ) -> Result<Answer, Failed> {
        let mut passes = std::mem::take(&mut self.trace);
        let answered = self.traced(id, asked, &mut passes);
        // A live run's pass, if dropped, is made again at its page's next poll, which reads every
        // exchange of its caller. A pass of a fold that the view dropped is made again anyway.
        let ended = |pass: &Trace| {
            let held = self.held.get(&pass.run);
            !held.is_some_and(|held| held.made == pass.made && held.ending.is_none())
        };
        while passes.iter().filter(|pass| ended(pass)).count() > TRACE_PASSES
            && let Some(oldest) = passes.iter().position(ended)
        {
            passes.remove(oldest);
        }
        self.trace = passes;
        answered
    }

    /// The answer to `asked`, of the pass of `passes` that it reads, made again unless it was
    /// read from the fold of the run that the view holds, and held as the one read last.
    fn traced(
        &mut self,
        id: &provider::RunTag,
        asked: Asked,
        passes: &mut Vec<Trace>,
    ) -> Result<Answer, Failed> {
        let held = self.held(id)?;
        let (caller, since, log_epoch) = match &asked {
            Asked::Steps {
                caller,
                since,
                log_epoch,
            } => (known(id, held, *caller)?, *since, *log_epoch),
            Asked::Entries { caller, .. }
            | Asked::Entry { caller, .. }
            | Asked::Search { caller, .. }
            | Asked::Stats { caller, .. } => (known(id, held, *caller)?, None, None),
            Asked::Diff { a, b, .. } => (caller_of(id, held, *a, *b)?, None, None),
            Asked::Exchange {
                request,
                part: None,
            } => (caller_of(id, held, *request, *request)?, None, None),
            Asked::Exchange {
                request,
                part: Some(part),
            } => {
                return exchange_bytes(&held.record, &held.exchanges, *request, *part);
            }
            Asked::Ops { caller } => {
                return Ok(Answer::Json(held.ops.json(known(id, held, *caller)?)));
            }
            Asked::Item { digest } => {
                let bytes = held.record.kept_item(digest).map_err(Failed::NotFound)?;
                return Ok(Answer::Bytes(Response::bytes(
                    "application/json; charset=utf-8",
                    bytes,
                )));
            }
            Asked::Payload { digest } => {
                let bytes = held.record.payload(digest).map_err(Failed::NotFound)?;
                return Ok(Answer::Bytes(Response::bytes(
                    "application/octet-stream",
                    bytes,
                )));
            }
        };
        if let Some(since) = since {
            line_at(held, since, log_epoch)?;
        } else if log_epoch.is_some() {
            line_at(held, 0, log_epoch)?;
        }
        let at = (passes.iter()).position(|pass| pass.run == id.as_str() && pass.caller == caller);
        let mut trace = at.map(|at| passes.remove(at));
        // A pass of a fold that the view dropped took a log that may have started over since.
        if !trace
            .as_ref()
            .is_some_and(|pass| pass.log_epoch == held.log_epoch && pass.made == held.made)
        {
            trace = None;
        }
        let mut trace =
            trace.unwrap_or_else(|| Trace::new(id.as_str(), caller, held.log_epoch, held.made));
        trace.extend(&held.record, &held.exchanges);
        let answer = trace.answer(held, asked);
        passes.push(trace);
        answer.map(Answer::Json)
    }
}

impl Trace {
    /// The answer to `asked`, of the pass, read from the fold `held`.
    fn answer(&self, held: &Held, asked: Asked) -> Result<Value, Failed> {
        let exchanges = &held.exchanges;
        let unpriced = BTreeMap::new();
        let prices =
            (held.readout.progress.as_ref()).map_or(&unpriced, |progress| &progress.token_prices);
        match asked {
            Asked::Steps { since, .. } => {
                let mut steps = self.steps_json(exchanges, since);
                steps["end"] = json!(held.fold.taken());
                steps["log_epoch"] = json!(held.log_epoch);
                steps["live"] = json!(held.ending.is_none());
                Ok(steps)
            }
            Asked::Entries {
                from,
                to,
                entry,
                kind,
                thread,
                ..
            } => Ok(self.entries_json(from, to, entry, kind, thread)),
            Asked::Entry {
                request, address, ..
            } => self.entry_json(request, &address),
            Asked::Search {
                query,
                kind,
                thread,
                ..
            } => Ok(self.search_json(&query, kind, thread)),
            Asked::Stats { by, .. } => Ok(self.stats_json(exchanges, prices, by)),
            Asked::Diff { a, b, part } => Ok(self.diff_json(&held.record, a, b, part)),
            Asked::Exchange { request, .. } => {
                let mut said = exchange_json(&held.record, exchanges, request);
                said["stats"] = self.step_stats(exchanges, prices, self.at[&request]);
                Ok(said)
            }
            Asked::Item { .. } | Asked::Payload { .. } | Asked::Ops { .. } => {
                unreachable!("answered above, without the pass")
            }
        }
    }
}

/// `caller`, refused unless it is the agent, or a job that run `id`'s log, held as `held`, names:
/// as a call it started, or as the caller of a model request or an op.
fn known(id: &provider::RunTag, held: &Held, caller: CallerName) -> Result<CallerName, Failed> {
    let progress = held.readout.progress.as_ref();
    let named = match caller {
        CallerName::Agent(_) => {
            progress.is_some_and(|progress| progress.agents.contains_key(&caller))
        }
        CallerName::Job(job) => {
            progress.is_some_and(|progress| progress.calls.iter().any(|call| call.job == job))
                || held
                    .exchanges
                    .by_request
                    .values()
                    .any(|said| said.caller == caller)
                || held.ops.names(caller)
        }
    };
    if !named {
        let reason = match caller {
            CallerName::Job(job) => format!(
                "run {}'s log names no job {job}: name an agent or a job of the run",
                id.as_str()
            ),
            CallerName::Agent(_) => format!(
                "run {} has no caller `{caller}`: name one of its agents or jobs",
                id.as_str()
            ),
        };
        return Err(Failed::NotFound(reason));
    }
    Ok(caller)
}

/// The caller of requests `a` and `b` of run `id`, held as `held`, or the refusal of two
/// requests of different callers.
fn caller_of(id: &provider::RunTag, held: &Held, a: u64, b: u64) -> Result<CallerName, Failed> {
    let caller_of = |request: u64| {
        let said = held.exchanges.by_request.get(&request);
        said.map(|said| said.caller).ok_or_else(|| {
            Failed::NotFound(format!(
                "run {}'s log names no model request {request}",
                id.as_str()
            ))
        })
    };
    let caller = caller_of(a)?;
    if caller_of(b)? != caller {
        return Err(Failed::Refused(Response::refused(
            "400 Bad Request",
            &format!(
                "request {a} is of {caller} and request {b} of another caller: expected two \
                 requests of one trace"
            ),
        )));
    }
    Ok(caller)
}

/// Request `request`'s `part` of `exchanges`, of `record`: its bytes as kept, or the request
/// rebuilt.
fn exchange_bytes(
    record: &Record,
    exchanges: &Exchanges,
    request: u64,
    part: Side,
) -> Result<Answer, Failed> {
    if !exchanges.by_request.contains_key(&request) {
        return Err(Failed::NotFound(format!(
            "run {}'s log names no model request {request}",
            record.id()
        )));
    }
    let bytes = match part {
        Side::Request => record.model_request(request),
        Side::Response => record.model_response(request),
    };
    let bytes = bytes.map_err(Failed::NotFound)?;
    Ok(Answer::Bytes(Response::bytes(
        "application/octet-stream",
        bytes,
    )))
}
