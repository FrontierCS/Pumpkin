//! The JSON forms of a trace's answers: its steps, entries, searches, exchanges and the parts of a
//! request as the record kept them.

use std::collections::BTreeSet;

use route::read::{self, Item};
use runtime::{KeptPart, KeptRequest, Record};
use schema::ir::Api;
use serde_json::{Map, Value, json};

use super::super::Failed;
use super::super::json::cut;
use super::{Address, Exchanges, Form, Said, Step, Trace, api_of, diff, stats};

/// The most bytes of one entry's text that `entry_json` gives; `text_bytes` gives the whole.
const ENTRY_TEXT_BYTES: usize = 1 << 20;

/// The most entries in one reply of `entries_json`, which says where the next begins.
const ENTRIES_WINDOW_ENTRIES: usize = 500;

/// The most hits that `search_json` gives, of however many it counts.
const SEARCH_HITS: usize = 1000;

/// The bytes of text on each side of a hit that `search_json` gives.
const SEARCH_CONTEXT_BYTES: usize = 80;

/// How many steps' entries one chunk of searchable text holds.
///
/// A page searches a static copy of a trace by reading every chunk once, so fewer, larger chunks
/// cost fewer requests; a live run's copy rewrites only its last chunk as it grows, so smaller ones
/// cost fewer bytes.
const SEARCH_CHUNK_STEPS: usize = 256;

/// The columns of the steps reply, which holds one array for each, in this order: row `i` of each
/// is step `i`.
const STEP_COLUMNS: [&str; 36] = [
    "request",
    "thread",
    "parent",
    "turn",
    "retry_of",
    "retry_count",
    "repeat_of",
    "start_ns",
    "first_byte_ns",
    "first_token_ns",
    "end_ns",
    "model_ns",
    "answer",
    "status",
    "failed",
    "upstream",
    "api",
    "input",
    "cached",
    "cache_hit",
    "cache_write",
    "output",
    "reasoning",
    "sent_bytes",
    "kept_bytes",
    "back_bytes",
    "items",
    "added",
    "added_entries",
    "answers",
    "calls",
    "tools",
    "differs",
    "stop",
    "whole",
    "notes",
];

fn time(value: Option<u64>) -> Value {
    value.map_or(Value::Null, |ns| json!(ns.to_string()))
}

impl Trace {
    /// The steps' columns, with the thread of each and its span, and the tool lists that they name.
    ///
    /// With `since`, only the steps that started, were answered or were derived anew at a log byte
    /// of `since` or later. The answers of a step in flight grow with no line of the log, so its
    /// row is sent again only once a line answers it. Column `tools` indexes `tool_lists` in the
    /// order this view read the lists, so it is stable within one session only: one view's process
    /// and `log_epoch`.
    pub(super) fn steps_json(&self, exchanges: &Exchanges, since: Option<u64>) -> Value {
        let mut columns: Vec<Vec<Value>> = vec![Vec::new(); STEP_COLUMNS.len()];
        let mut threads = BTreeSet::new();
        for &index in self.at.values() {
            let step = &self.steps[index];
            let Some(said) = exchanges.by_request.get(&step.request) else {
                continue;
            };
            let started = said.started;
            let answered = said.answered.as_ref();
            if let Some(since) = since {
                let late = |byte: u64| byte >= since;
                if !started.is_some_and(|(byte, _)| late(byte))
                    && !answered.is_some_and(|(byte, _, _)| late(*byte))
                    && !step.changed.is_some_and(late)
                {
                    continue;
                }
            }
            // Of the threads of the rows sent.
            threads.extend(step.thread);
            let row = self.row(exchanges, step, said);
            for (column, value) in columns.iter_mut().zip(row) {
                column.push(value);
            }
        }
        let steps: Map<String, Value> = (STEP_COLUMNS.iter())
            .zip(columns)
            .map(|(name, column)| ((*name).to_owned(), Value::Array(column)))
            .collect();
        let tools: Vec<Value> = (self.tools.iter())
            .map(|tools| json!({ "digest": tools.digest, "names": tools.names }))
            .collect();
        let count = (self.steps.iter())
            .filter(|step| step.thread == Some(step.request))
            .count();
        json!({
            "caller": self.caller,
            "threads": count,
            "thread_spans": self.thread_spans(exchanges, &threads),
            "steps": steps,
            "tool_lists": tools,
        })
    }

    fn row(&self, exchanges: &Exchanges, step: &Step, said: &Said) -> [Value; STEP_COLUMNS.len()] {
        let (started, answered) = (said.started, said.answered.as_ref());
        let event = &said.event();
        let request_of = |other: Option<usize>| json!(other.map(|other| self.steps[other].request));
        let (retry_of, repeat_of) = self.same(exchanges, step);
        let usage = &event["usage"];
        let calls: Vec<&str> = (step.answer.iter())
            .filter_map(|item| match item {
                Item::Call { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        let parent_len = step
            .parent
            .map_or(0, |parent| self.steps[parent].list.len());
        let placed = (step.placed && step.thread.is_some()).then_some(());
        // A body kept whole holds no list apart, so it sent items that no list counts.
        let listed = placed.filter(|()| match step.form {
            Some(Form::Apart) => true,
            Some(Form::Whole) | None => false,
        });
        let cache_hit = (usage["cached_tokens"].as_u64())
            .zip(usage["input_tokens"].as_u64())
            .and_then(|(cached, input)| stats::share(cached, input));
        [
            json!(step.request),
            json!(step.thread),
            request_of(step.parent),
            json!(step.turn),
            json!(retry_of),
            json!(self.retry_count(exchanges, step)),
            json!(repeat_of),
            time(started.and_then(|(_, ns)| ns)),
            event["first_byte_ns"].clone(),
            event["first_token_ns"].clone(),
            time(answered.and_then(|(_, ns, _)| *ns)),
            json!(stats::span(
                started.and_then(|(_, ns)| ns),
                answered.and_then(|(_, ns, _)| *ns)
            )),
            event["answer"]["kind"].clone(),
            event["upstream_status"].clone(),
            event["failed"].clone(),
            event["upstream"].clone(),
            event["api"].clone(),
            usage["input_tokens"].clone(),
            usage["cached_tokens"].clone(),
            json!(cache_hit),
            usage["cache_write_tokens"].clone(),
            usage["output_tokens"].clone(),
            usage["reasoning_tokens"].clone(),
            event["kept"]["request"]["body_bytes"].clone(),
            event["kept"]["request"]["bytes"].clone(),
            event["kept"]["response"]["bytes"].clone(),
            json!(listed.map(|()| step.list.len())),
            json!(listed.map(|()| step.list.len().saturating_sub(parent_len))),
            json!(placed.map(|()| step.added.len())),
            json!(step.answer.len()),
            json!(calls),
            json!(step.tools),
            json!(self.differs(step)),
            json!(step.stop),
            json!(step.whole),
            if step.notes.is_empty() {
                Value::Null
            } else {
                json!(step.notes)
            },
        ]
    }

    /// The earlier request that sent `step`'s list last before it, as `(retry_of, repeat_of)`: a
    /// retry if that request did not succeed, else a repeat.
    pub(super) fn same(&self, exchanges: &Exchanges, step: &Step) -> (Option<u64>, Option<u64>) {
        match step.same_as {
            Some(other) if self.answered_ok(exchanges, other) == Some(false) => {
                (Some(self.steps[other].request), None)
            }
            Some(other) => (None, Some(self.steps[other].request)),
            None => (None, None),
        }
    }

    /// Which retry `step` is, counting from 1, of the first of the requests it retries, each a
    /// retry of the one before; `None` for a step that is no retry.
    fn retry_count(&self, exchanges: &Exchanges, step: &Step) -> Option<u64> {
        let mut count = 0;
        let mut at = step;
        while let (Some(of), _) = self.same(exchanges, at) {
            count += 1;
            at = &self.steps[self.at[&of]];
        }
        (count > 0).then_some(count)
    }

    /// Which of `system` and `tools` `step` sent other than its parent did; `None` for a step with
    /// no parent.
    fn differs(&self, step: &Step) -> Option<Vec<&'static str>> {
        let parent = &self.steps[step.parent?];
        let system = step.key(diff::Compared::System) != parent.key(diff::Compared::System);
        let differs = [(system, "system"), (step.tools != parent.tools, "tools")];
        Some(
            differs
                .into_iter()
                .filter(|(differs, _)| *differs)
                .map(|(_, part)| part)
                .collect(),
        )
    }

    /// Whether step `index`'s request was answered by a forwarded 2xx with no failure, or `None`
    /// if it has no answer yet.
    pub(super) fn answered_ok(&self, exchanges: &Exchanges, index: usize) -> Option<bool> {
        let said = exchanges.by_request.get(&self.steps[index].request)?;
        said.answered.as_ref()?;
        let event = said.event();
        let status = event["upstream_status"].as_u64();
        Some(
            event["answer"]["kind"] == "forwarded"
                && status.is_some_and(|status| (200..300).contains(&status))
                && event["failed"].is_null(),
        )
    }

    fn entries_of<'s>(
        &'s self,
        step: &'s Step,
    ) -> impl Iterator<
        Item = (
            usize,
            Address,
            Result<&'s Item, &'s String>,
            Option<&'s str>,
        ),
    > {
        let added = step.added.iter().map(|(address, part, at)| {
            let part = &self.parts[*part as usize];
            let item = part.entries.as_ref().map(|entries| entries.get(*at));
            (address.clone(), item, part.digest.as_deref())
        });
        let answer = (step.answer.iter().enumerate())
            .map(|(at, item)| (Address::Out(at), Ok(Some(item)), None));
        let entries = added.chain(answer).enumerate();
        entries.filter_map(|(at, (address, item, digest))| match item {
            Ok(Some(item)) => Some((at, address, Ok(item), digest)),
            Ok(None) => None,
            Err(error) => Some((at, address, Err(error), digest)),
        })
    }

    fn searched<'s>(
        &'s self,
        step: &'s Step,
    ) -> impl Iterator<Item = (usize, Address, &'static str, String)> + 's {
        self.entries_of(step).map(|(at, address, item, _)| {
            let text = match item {
                Ok(item) => item_json(item)["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                Err(error) => error.clone(),
            };
            (at, address, item.map_or("unread", kind_name), text)
        })
    }

    /// The places, in the entries of the steps of `thread` (every thread if `None`) and of `kind`
    /// (every kind if `None`), where `query` is found with the case ignored.
    pub(super) fn search_json(
        &self,
        query: &str,
        kind: Option<&str>,
        thread: Option<u64>,
    ) -> Value {
        let query = searchable(query);
        let (mut count, mut hits) = (0_usize, Vec::new());
        for step in self.at.values().map(|&index| &self.steps[index]) {
            if query.is_empty() || thread.is_some_and(|thread| step.thread != Some(thread)) {
                continue;
            }
            for (at, address, kind_of, text) in self.searched(step) {
                if kind.is_some_and(|kind| kind != kind_of) {
                    continue;
                }
                for (found, _) in searchable(&text).match_indices(&query) {
                    count += 1;
                    if hits.len() < SEARCH_HITS {
                        let end = found + query.len();
                        let before =
                            text.floor_char_boundary(found.saturating_sub(SEARCH_CONTEXT_BYTES));
                        let after =
                            text.ceil_char_boundary((end + SEARCH_CONTEXT_BYTES).min(text.len()));
                        hits.push(json!({
                            "at": address.of(step.request), "request": step.request, "index": at,
                            "kind": kind_of, "byte": found,
                            "context": [&text[before..found], &text[found..end], &text[end..after]],
                        }));
                    }
                }
            }
        }
        json!({ "count": count, "hits": hits })
    }

    /// Chunk `chunk` of the text that a search reads: the entries of [`SEARCH_CHUNK_STEPS`] steps,
    /// each as `[index, at, kind, text]` with the text in its [`searchable`] form and an entry with
    /// no text left out, and the next chunk, if there is one. [`Self::search_json`] finds a query
    /// in an entry where it is a substring of that text, at the same byte.
    pub(super) fn search_chunk_json(&self, chunk: usize) -> Value {
        let first = chunk.saturating_mul(SEARCH_CHUNK_STEPS);
        let chunked = self.at.values().skip(first).take(SEARCH_CHUNK_STEPS);
        let steps: Vec<Value> = chunked
            .map(|&index| {
                let step = &self.steps[index];
                let entries: Vec<Value> = (self.searched(step))
                    .filter(|(.., text)| !text.is_empty())
                    .map(|(at, address, kind, text)| {
                        json!([at, address.of(step.request), kind, searchable(&text)])
                    })
                    .collect();
                json!({ "request": step.request, "thread": step.thread, "entries": entries })
            })
            .collect();
        let more = self.at.len() > first.saturating_add(SEARCH_CHUNK_STEPS);
        json!({ "steps": steps, "next": more.then_some(chunk + 1) })
    }

    /// The entries of the requests `from` to `to`, starting at entry `entry` of the first, with
    /// each string cut ([`cut`]).
    pub(super) fn entries_json(
        &self,
        from: u64,
        to: u64,
        entry: usize,
        kind: Option<&str>,
        thread: Option<u64>,
    ) -> Value {
        let mut entries = Vec::new();
        let mut next = Value::Null;
        'steps: for (&request, &index) in self.at.range(from..=to) {
            let step = &self.steps[index];
            if thread.is_some_and(|thread| step.thread != Some(thread)) {
                continue;
            }
            let first = if request == from { entry } else { 0 };
            let entries_of = self.entries_of(step).filter(|(at, ..)| *at >= first);
            for (at, address, item, digest) in entries_of {
                let kind_of = item.map_or("unread", kind_name);
                if kind.is_some_and(|kind| kind != kind_of) {
                    continue;
                }
                if entries.len() == ENTRIES_WINDOW_ENTRIES {
                    next = json!({ "request": request, "entry": at });
                    break 'steps;
                }
                entries.push(cut(self.entry_of(request, at, &address, item, digest)));
            }
        }
        json!({ "entries": entries, "next": next })
    }

    /// The entry at `address` of `request`, whole up to [`ENTRY_TEXT_BYTES`].
    pub(super) fn entry_json(&self, request: u64, address: &Address) -> Result<Value, Failed> {
        let step = self.at.get(&request).map(|&index| &self.steps[index]);
        let found =
            step.and_then(|step| (self.entries_of(step)).find(|(_, other, ..)| other == address));
        let Some((at, address, item, digest)) = found else {
            return Err(Failed::NotFound(format!(
                "{}'s trace has no entry at `{}`",
                self.caller,
                address.of(request)
            )));
        };
        let mut json = self.entry_of(request, at, &address, item, digest);
        if let Some(Value::String(text)) = json.get_mut("text") {
            let bytes = text.len();
            text.truncate(text.floor_char_boundary(ENTRY_TEXT_BYTES));
            json["text_bytes"] = json!(bytes);
        }
        Ok(json)
    }

    fn entry_of(
        &self,
        request: u64,
        at: usize,
        address: &Address,
        item: Result<&Item, &String>,
        digest: Option<&str>,
    ) -> Value {
        let kind_of = item.map_or("unread", kind_name);
        let address = address.of(request);
        let side = if matches!(address_side(&address), "out") {
            "answer"
        } else {
            "added"
        };
        let mut json = match item {
            Ok(item) => item_json(item),
            Err(error) => json!({ "text": error }),
        };
        json["at"] = json!(address);
        json["request"] = json!(request);
        json["index"] = json!(at);
        json["side"] = json!(side);
        json["kind"] = json!(kind_of);
        json["digest"] = json!(digest);
        if let Some(other) = self.pairs.get(&address) {
            let name = if matches!(item, Ok(Item::Call { .. })) {
                "output"
            } else {
                "call"
            };
            json[name] = json!(other);
        }
        json
    }
}

/// The form of `text` that a search matches in, and of the query that it matches: its ASCII
/// letters lower-cased, so that a query matches letters of either case, each byte where it is in
/// `text`.
pub(in crate::view) fn searchable(text: &str) -> String {
    text.to_ascii_lowercase()
}

fn address_side(address: &str) -> &str {
    address.split('/').nth(1).unwrap_or_default()
}

fn kind_name(item: &Item) -> &'static str {
    match item {
        Item::System { .. } => "system",
        Item::User { .. } => "user",
        Item::Assistant { .. } => "assistant",
        Item::Reasoning { .. } => "reasoning",
        Item::Call { .. } => "call",
        Item::Output { .. } => "output",
        Item::Other { .. } => "other",
    }
}

fn item_json(item: &Item) -> Value {
    match item {
        Item::System { text }
        | Item::User { text }
        | Item::Assistant { text }
        | Item::Reasoning { text } => json!({ "text": text }),
        Item::Call {
            id,
            name,
            arguments,
        } => json!({ "call_id": id, "name": name, "text": arguments }),
        Item::Output {
            call_id,
            text,
            error,
        } => {
            let mut json = json!({ "call_id": call_id, "text": text });
            if *error {
                json["error"] = json!(true);
            }
            json
        }
        Item::Other { json } => json!({ "text": json.to_string() }),
    }
}

/// A request as the record kept it: its conversation's items and its other fields, each by its
/// digest or as sent. `conversation` is `null` if the record kept no items apart.
pub(super) fn kept_json(kept: KeptRequest) -> Value {
    let KeptRequest {
        conversation,
        fields,
    } = kept;
    let part = |part: KeptPart| match part {
        KeptPart::Kept(digest) => json!({ "item": digest.hex() }),
        KeptPart::Sent(value) => json!({ "sent": value }),
    };
    let conversation = conversation.map(|(field, items)| {
        let items: Vec<Value> = items.into_iter().map(part).collect();
        json!({ "field": field, "items": items })
    });
    let fields: Vec<Value> = (fields.into_iter())
        .map(|(name, kept)| {
            let mut field = part(kept);
            field["name"] = json!(name);
            field
        })
        .collect();
    json!({ "conversation": conversation, "fields": fields })
}

/// What the log says of request `request`, and its configuration read from the record.
pub(super) fn exchange_json(record: &Record, exchanges: &Exchanges, request: u64) -> Value {
    let said = &exchanges.by_request[&request];
    let event = &said.event();
    let api = api_of(event);
    let config = config_json(record, request, api, said.answered.is_some());
    json!({
        "request": request,
        "caller": said.caller,
        "api": event["api"],
        "upstream": event["upstream"],
        "status": event["upstream_status"],
        "answer": event["answer"],
        "failed": event["failed"],
        "request_id": event["request_id"],
        "usage": event["usage"],
        "times": {
            "start_ns": time(said.started.and_then(|(_, ns)| ns)),
            "first_byte_ns": event["first_byte_ns"],
            "first_token_ns": event["first_token_ns"],
            "end_ns": time(said.answered.as_ref().and_then(|(_, ns, _)| *ns)),
        },
        "kept": event["kept"],
        "config": config,
    })
}

fn config_json(record: &Record, request: u64, api: Option<Api>, answered: bool) -> Value {
    match (api, answered) {
        (Some(api), true) => match record.model_request(request) {
            Ok(body) => {
                let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let read::Config {
                    model,
                    reasoning_effort,
                    temperature,
                    max_tokens,
                    stop,
                    stream,
                    tools,
                } = read::config(api, &body);
                json!({
                    "model": model, "reasoning_effort": reasoning_effort,
                    "temperature": temperature, "max_tokens": max_tokens, "stop": stop,
                    "stream": stream, "tools": tools,
                })
            }
            Err(error) => json!({ "error": error }),
        },
        (None, true) => json!({ "error": "its API is not named, so its body is not read" }),
        (_, false) => {
            json!({ "error": "in flight: the record names its body's parts once answered" })
        }
    }
}
