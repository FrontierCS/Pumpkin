//! The JSON forms of a trace's answers: its steps as columns, windows and searches of its
//! entries, one entry, and what the log says of an exchange. What each answer reads is the
//! parent module's; which request it answers, [`ask`](super::ask)'s.

use std::collections::BTreeSet;

use route::read::{self, Item};
use runtime::{KeptPart, KeptRequest, Record};
use schema::ir::Api;
use serde_json::{Map, Value, json};

use super::super::Failed;
use super::super::json::cut;
use super::{Address, Exchanges, Said, Step, Trace, api_of, diff, stats};

/// The most bytes of the text that the `entry` reply gives of one entry, past which it cuts the
/// text and says its length: a judge's prompt, the largest entry a run's record holds, is tens
/// of KiB.
const ENTRY_TEXT_BYTES: usize = 1 << 20;

/// The most entries that a window of a trace's entries holds.
const ENTRIES_WINDOW_ENTRIES: usize = 500;

/// The most places of a search that its answer lists; it counts them all.
const SEARCH_HITS: usize = 1000;

/// The most bytes of an entry's text that a place of a search shows on each side of it.
const SEARCH_CONTEXT_BYTES: usize = 80;

/// How many steps a chunk of a trace's searchable text holds ([`Trace::search_chunk_json`]).
// A page searches a static copy of a trace by reading every chunk once, so fewer, larger chunks
// cost fewer requests; a live run's copy rewrites only its last chunk as it grows, so smaller ones
// cost fewer bytes.
const SEARCH_CHUNK_STEPS: usize = 256;

/// The columns of a trace's steps.
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

/// The JSON of `value`, an `Option` of times in nanoseconds, as the log's decimal strings.
fn time(value: Option<u64>) -> Value {
    value.map_or(Value::Null, |ns| json!(ns.to_string()))
}

impl Trace {
    /// The steps, as columns of equal length, in the order of their requests; with `since`, those
    /// of the steps that a line at or past byte `since` started, answered or changed. Column
    /// `added` counts the items past the parent's list, `added_entries` the entries `entries`
    /// gives of them (less those sending back the parent's answer, with the fields that differ),
    /// `cache_hit` the fraction of the input tokens cached, and `answers` a step's answer
    /// entries; of a step in flight, those so far, which grow with no line of the log, so `since`
    /// sends its row again only once a line answers it. Column `tools` indexes `tool_lists` in the
    /// order this view read the lists, so it is stable within one session only: one view's
    /// process and `log_epoch`.
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
            // Of the threads of the rows sent.
            "thread_spans": self.thread_spans(exchanges, &threads),
            "steps": steps,
            "tool_lists": tools,
        })
    }

    /// The row of `step`, of which the log said `said`, in the order of [`STEP_COLUMNS`].
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
            json!(placed.map(|()| step.list.len())),
            json!(placed.map(|()| step.list.len().saturating_sub(parent_len))),
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

    /// The requests that `step` is a retry of, and a repeat of: of the step that sent its list
    /// last before it, as that one failed or not.
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

    /// Which of `system` and `tools` `step` sent other than its parent did; `None` for a step
    /// with no parent.
    fn differs(&self, step: &Step) -> Option<Vec<&'static str>> {
        let parent = &self.steps[step.parent?];
        let system =
            self.key(step, diff::Compared::System) != self.key(parent, diff::Compared::System);
        let differs = [(system, "system"), (step.tools != parent.tools, "tools")];
        Some(
            differs
                .into_iter()
                .filter(|(differs, _)| *differs)
                .map(|(_, part)| part)
                .collect(),
        )
    }

    /// Whether step `index`'s request was answered with a success; `None` while in flight.
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

    /// The entries of `step`, each with its index among them, its address, the entry or why it
    /// can't be read, and its digest if it is a kept item's: what it added, then its answer.
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

    /// The entries of `step` that a search reads, each with its index among them, its address,
    /// its kind and its text, or why it can't be read.
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

    /// The places where `query` is, in the text of an entry of `kind` and of thread `thread` if
    /// given, as [`searchable`] matches them: their count, and the first [`SEARCH_HITS`], each
    /// with [`SEARCH_CONTEXT_BYTES`] of the text on each side.
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

    /// Chunk `chunk` of the text that a search reads, the entries of [`SEARCH_CHUNK_STEPS`] steps
    /// in the order of their requests, as JSON: `{"steps": [{"request": n, "thread": t,
    /// "entries": [[index, at, kind, text], ...]}], "next": c}`, each text in its [`searchable`]
    /// form, an entry with no text left out, and `c` the next chunk, or `null` after the last.
    /// [`Self::search_json`] finds a query in an entry where it is a substring of that text, at
    /// the same byte.
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

    /// The entries of the steps of requests `from` to `to`, of thread `thread` and of `kind` if
    /// given, from the `entry`th of the first on, at most [`ENTRIES_WINDOW_ENTRIES`]: each step's
    /// added entries, then its answer's.
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

    /// The entry of step `request`'s at `address`, uncut up to [`ENTRY_TEXT_BYTES`] of its text,
    /// with `text_bytes`, the length of its text whole; or the refusal of an address that names
    /// none.
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

    /// The entry `at` of step `request`'s, at `address`, as JSON: `item`'s fields, or why it
    /// can't be read, with where it is, its kind, its digest and its call's or output's address.
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

/// The side an address names: `head`, `in` or `out`.
fn address_side(address: &str) -> &str {
    address.split('/').nth(1).unwrap_or_default()
}

/// The name of `item`'s kind.
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

/// `item`'s own fields, as JSON.
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

/// `kept`, a request's body as the record keeps it, as JSON: `{"conversation": {"field": f,
/// "items": [part, ...]}, "fields": [{"name": n, ...part}, ...]}`, each part `{"item": hex}`, kept
/// apart as the item that `hex` names, or `{"sent": v}`, held as sent, and `conversation` `null`
/// if the record kept no items apart. The body is `fields`, each part's value in place, with `f`
/// holding `items`' values in their order.
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

/// What the log says of request `request` of `exchanges`, of `record`, which it names, and what
/// its body asked, as JSON.
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

/// What request `request` of `record`, of `api`, asked of the model, if `answered`, as JSON, or
/// why it can't be read.
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
