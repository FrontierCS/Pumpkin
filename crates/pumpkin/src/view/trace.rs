//! A caller's trace: its model requests as steps of its conversations, read in one pass over its
//! exchanges and held, extended as the log grows.
//!
//! A request sends its conversation again, as a list of items, each of which the record keeps
//! once, by its digest. A request whose list extends another's is that one's child, so the
//! requests of a caller make a forest: each tree is a thread, named by its first root's request.
//! Of the earlier requests that sent the longest such list, the parent is the last whose answer
//! the request continues ([`Trace::continues`]), else the last. A request that sends a list an
//! earlier one sent is a retry of it if that one failed, and a repeat of it if it was answered.
//! The forest is that of the requests in their order, whatever the order of their answers. A step
//! shows what its request added past its parent's list, less what only sends back its parent's
//! answer, and its answer, as `route::read` reads either API's; a turn starts at a step that adds
//! a user's item. A tool call of an answer is paired with each of its outputs, in later steps of
//! the thread, by the call's id, and names the first.
//!
//! The pass reads each request's kept parts without the items they name, each item once, and each
//! answer once it has ended; a step in flight shows its answer so far, read again at each
//! extension. A request whose files can't be read is a step with what the log says of it and a
//! note. Which parts a record keeps apart, its log says in a request's answer, so a step in flight
//! is in no thread until it is answered.
//!
//! Which request asks for what, and the answer it gets, is [`ask`]'s; the JSON forms of the
//! answers, [`json`](mod@json)'s; a trace's statistics, [`stats`]'s; the diff of two of its
//! requests, [`diff`]'s; and the ops the run brokered for its caller, [`ops`]'s.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use route::read::{self, Item};
use runtime::{KeptPart, KeptRequest, Record};
use schema::ir::Api;
use serde_json::{Map, Value, json};

use super::json::cut;

mod ask;
mod diff;
mod json;
mod ops;
mod stats;

pub(super) use ask::{Answer, Asked, asked};
pub(super) use ops::Ops;

/// The most traces that the view holds, those read last, besides those of live runs whose folds
/// it holds: a page shows one, and a person opens a few side by side.
pub(super) const TRACE_PASSES: usize = 3;

/// The most steps up a thread that an output's call is looked for in, past its own step's parent:
/// a harness sends a call's output in the request after the call's answer.
const CALL_LOOKUP_STEPS: usize = 64;

/// The model exchanges that a run's log names, by request, as its lines are taken.
#[derive(Default)]
pub(super) struct Exchanges {
    by_request: BTreeMap<u64, Said>,
    /// The request that each of the log's model lines names, in the log's order.
    lines: Vec<u64>,
}

/// A model request, as the log says of it so far.
struct Said {
    caller: String,
    /// The byte of its `model_request_started`, and when it was observed, if logged.
    started: Option<(u64, Option<u64>)>,
    /// The byte of its `model_answered`, when it was observed, and the event's JSON, held as text
    /// as a parsed value would hold ten times the memory.
    answered: Option<(u64, Option<u64>, Box<str>)>,
}

impl Said {
    /// Its `model_answered`'s event; `null` while in flight.
    fn event(&self) -> Value {
        let text = self.answered.as_ref().map(|(_, _, text)| &**text);
        text.and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(Value::Null)
    }
}

impl Exchanges {
    /// Takes `record`, the line of the log at byte `byte`.
    pub(super) fn take(&mut self, byte: u64, record: &Value) {
        let event = &record["event"];
        let answered = match event["kind"].as_str() {
            Some("model_request_started") => false,
            Some("model_answered") => true,
            _ => return,
        };
        let (Some(request), Some(caller)) = (event["request"].as_u64(), event["caller"].as_str())
        else {
            return;
        };
        // When the request's thread observed it, else when the log took it, as a timeline orders
        // the log's records.
        let ns = nanos(&record["observed_ns"]).or_else(|| nanos(&record["at_ns"]));
        let said = self.by_request.entry(request).or_insert_with(|| Said {
            caller: caller.to_owned(),
            started: None,
            answered: None,
        });
        if answered {
            said.answered = Some((byte, ns, event.to_string().into()));
        } else {
            said.started = Some((byte, ns));
        }
        self.lines.push(request);
    }
}

/// The API of the request whose `model_answered` is `event`, if the log names it.
fn api_of(event: &Value) -> Option<Api> {
    match &event["api"] {
        // A build before the log named a request's API served the Responses API and Chat
        // Completions alone, whose conversations are their `input` and `messages`.
        Value::Null => match event["kept"]["request"]["items"]["field"].as_str() {
            Some("input") => Some(Api::Responses),
            Some("messages") => Some(Api::Chat),
            _ => None,
        },
        api => serde_json::from_value(api.clone()).ok(),
    }
}

/// A decimal string of nanoseconds, as the log writes a time.
fn nanos(value: &Value) -> Option<u64> {
    value.as_str()?.parse().ok()
}

/// A part of a request: an item of its conversation, or a field of its body, read once by its
/// digest or, as the record keeps it as sent, by its JSON.
struct Part {
    digest: Option<String>,
    /// The names of its tools, if it is a body's `tools`.
    tools: Option<Vec<String>>,
    /// Its entries, or why its item can't be read.
    entries: Result<Vec<Item>, String>,
}

/// A step of a trace: a request, and what is known of it.
struct Step {
    request: u64,
    /// Whether it was answered, and its request read, if it can be.
    placed: bool,
    /// How the record kept its request, if it was read, which places it in the forest.
    form: Option<Form>,
    api: Option<Api>,
    /// Its conversation, each item as the index of its part.
    list: Vec<u32>,
    /// The fields of its body, each as the index of its part.
    fields: BTreeMap<String, u32>,
    /// The part of its list of tools, if it sent one.
    tools_part: Option<u32>,
    parent: Option<usize>,
    thread: Option<u64>,
    /// The step that sent the same list last before it, by their requests.
    same_as: Option<usize>,
    /// The byte of the log's last line whose answer changed it once it was placed: of an earlier
    /// request, where it is in the forest; of a later one, the output that a call of its answer
    /// names.
    changed: Option<u64>,
    /// The request whose step began its turn.
    turn: Option<u64>,
    /// The entries it added, by address and part: of its fields, then of its items.
    added: Vec<(Address, u32, usize)>,
    /// Its answer's items, as far as read.
    answer: Vec<Item>,
    stop: Option<String>,
    whole: bool,
    tools: Option<usize>,
    notes: Vec<String>,
}

/// How the record kept a request's body.
#[derive(Clone, Copy)]
enum Form {
    /// Whole, so that its conversation is compared with no other.
    Whole,
    /// With its conversation's items apart.
    Apart,
}

/// Where a step is in the forest, and what it pairs, as a placement out of order may change it.
#[derive(PartialEq, Eq)]
struct Derived {
    thread: Option<u64>,
    parent: Option<u64>,
    turn: Option<u64>,
    same_as: Option<u64>,
    added: Vec<Address>,
    pairs: Vec<(String, String)>,
}

/// Where an entry is: `<request>/in/<item>/<part>`, the part of the item at that index of the
/// request's conversation, `<request>/head/<field>/<part>`, of a field its API reads as an item,
/// such as the Responses API's `instructions`, or `<request>/out/<part>`, of its answer; the same
/// in every reply, as a request's list and answer never change once read.
#[derive(Clone, PartialEq, Eq)]
pub(super) enum Address {
    Head(String, usize),
    In(usize, usize),
    Out(usize),
}

impl Address {
    /// The request and address that `text` names, as [`Address::of`] writes them.
    fn parse(text: &str) -> Option<(u64, Self)> {
        let number = |text: &str| text.parse::<usize>().ok();
        let address = match text.split('/').collect::<Vec<_>>()[..] {
            [request, "head", field, part] if !field.is_empty() => {
                (request, Self::Head(field.to_owned(), number(part)?))
            }
            [request, "in", item, part] => (request, Self::In(number(item)?, number(part)?)),
            [request, "out", part] => (request, Self::Out(number(part)?)),
            _ => return None,
        };
        Some((address.0.parse().ok()?, address.1))
    }

    fn of(&self, request: u64) -> String {
        match self {
            Self::Head(field, part) => format!("{request}/head/{field}/{part}"),
            Self::In(item, part) => format!("{request}/in/{item}/{part}"),
            Self::Out(part) => format!("{request}/out/{part}"),
        }
    }
}

/// A list of tools that a request declares, by its part.
struct Tools {
    digest: Option<String>,
    names: Vec<String>,
}

/// A caller's trace, as one pass over its exchanges made it, as far as the log was taken.
pub(super) struct Trace {
    run: String,
    caller: String,
    log_epoch: u64,
    /// Which of the view's folds of the run it was read from ([`super::Held::made`]).
    made: u64,
    /// How many of the log's model lines the pass took ([`Exchanges::lines`]).
    taken: usize,
    steps: Vec<Step>,
    /// Each step's index, by its request.
    at: BTreeMap<u64, usize>,
    parts: Vec<Part>,
    /// Each part's index, by its field, if it is one, and its digest or JSON.
    keys: BTreeMap<(Option<String>, String), u32>,
    /// The forest's nodes: each holds the steps whose list ends there, by the order placed.
    nodes: Vec<Vec<usize>>,
    /// Each node's child by the part that leads to it.
    edges: BTreeMap<(usize, u32), usize>,
    tools: Vec<Tools>,
    tools_of: BTreeMap<u32, usize>,
    /// Each call's output and each output's call, by their addresses.
    pairs: BTreeMap<String, String>,
    /// The API of the caller's answered requests, if they agree.
    api: Option<Api>,
    /// The last request placed in the forest, which places a later one after it.
    derived: Option<u64>,
}

impl Trace {
    fn new(run: &str, caller: &str, log_epoch: u64, made: u64) -> Self {
        Self {
            run: run.to_owned(),
            caller: caller.to_owned(),
            log_epoch,
            made,
            taken: 0,
            steps: Vec::new(),
            at: BTreeMap::new(),
            parts: Vec::new(),
            keys: BTreeMap::new(),
            nodes: vec![Vec::new()],
            edges: BTreeMap::new(),
            tools: Vec::new(),
            tools_of: BTreeMap::new(),
            pairs: BTreeMap::new(),
            api: None,
            derived: None,
        }
    }

    /// Takes what `exchanges`, of `record`, gained since the pass last took them, and reads again
    /// the answer so far of each step in flight.
    fn extend(&mut self, record: &Record, exchanges: &Exchanges) {
        let mut touched = Vec::new();
        for &request in &exchanges.lines[self.taken..] {
            let Some(said) = exchanges.by_request.get(&request) else {
                continue;
            };
            if said.caller != self.caller {
                continue;
            }
            let steps = &mut self.steps;
            let index = *self.at.entry(request).or_insert_with(|| {
                steps.push(Step::new(request));
                steps.len() - 1
            });
            touched.push(index);
        }
        self.taken = exchanges.lines.len();
        let mut placed = Vec::new();
        for index in touched {
            if self.update(index, record, exchanges) {
                placed.push(index);
            }
        }
        // After the answered steps, whose answers name the API that a step in flight is read by,
        // so that a pass reads it alike however much of the log it takes at a time.
        for index in 0..self.steps.len() {
            if !self.steps[index].placed {
                self.read_so_far(index, record);
            }
        }
        // In the order of their requests, so that a pass makes the same forest however much of
        // the log it takes at a time.
        placed.sort_by_key(|&index| self.steps[index].request);
        let first = placed.first().map(|&index| self.steps[index].request);
        if first.is_some_and(|first| self.derived.is_some_and(|last| first < last)) {
            let answered = |index: usize| {
                let said = exchanges.by_request.get(&self.steps[index].request);
                said.and_then(|said| said.answered.as_ref().map(|(byte, _, _)| *byte))
            };
            let byte = placed.iter().filter_map(|&index| answered(index)).max();
            self.derive_again(byte);
        } else {
            for index in placed {
                let said = exchanges.by_request.get(&self.steps[index].request);
                let byte = said.and_then(|said| said.answered.as_ref().map(|(byte, _, _)| *byte));
                self.derive(index, byte);
            }
        }
    }

    /// Reads the answer so far of step `index`, in flight, by the API its caller's answers have;
    /// nothing while no answer of the caller says which.
    fn read_so_far(&mut self, index: usize, record: &Record) {
        let Some(api) = self.api else {
            return;
        };
        let step = &mut self.steps[index];
        step.api = Some(api);
        if let Ok(bytes) = record.model_response(step.request) {
            let reply = read::answer(api, &bytes);
            (step.answer, step.stop, step.whole) = (reply.items, reply.stop, reply.whole);
        }
    }

    /// Reads step `index` once the log says it was answered; whether this call read its request,
    /// which is then to be placed in the forest.
    fn update(&mut self, index: usize, record: &Record, exchanges: &Exchanges) -> bool {
        let request = self.steps[index].request;
        let Some(said) = exchanges.by_request.get(&request) else {
            return false;
        };
        if said.answered.is_none() || self.steps[index].placed {
            return false;
        }
        let event = said.event();
        let kept = &event["kept"];
        let api = api_of(&event);
        let step = &mut self.steps[index];
        step.placed = true;
        step.api = api;
        self.api = match (self.api, api) {
            (None, api) => api,
            (Some(known), Some(api)) if known == api => Some(known),
            (Some(_), _) => None,
        };
        let Some(api) = api else {
            step.notes
                .push("its API is not named, so its bodies are not read".to_owned());
            return false;
        };
        match &kept["response"] {
            Value::Null => step.notes.push("its response was not kept".to_owned()),
            _ => match record.model_response(request) {
                Ok(bytes) => {
                    let reply = read::answer(api, &bytes);
                    (step.answer, step.stop, step.whole) = (reply.items, reply.stop, reply.whole);
                }
                Err(error) => step
                    .notes
                    .push(format!("its response can't be read: {error}")),
            },
        }
        if kept["request"].is_null() {
            step.notes.push("its request was not kept".to_owned());
            return false;
        }
        match record.kept_request(request, &kept["request"]) {
            Ok(kept) => {
                self.read(index, api, kept, record);
                true
            }
            Err(error) => {
                (self.steps[index].notes).push(format!("its request can't be read: {error}"));
                false
            }
        }
    }

    /// Reads the parts of step `index`'s request, which `kept` holds, of `api`.
    fn read(&mut self, index: usize, api: Api, kept: KeptRequest, record: &Record) {
        let KeptRequest {
            conversation,
            fields,
        } = kept;
        let fields: BTreeMap<String, u32> = (fields.into_iter())
            .map(|(field, part)| {
                let part = self.part(api, Some(&field), part, record);
                (field, part)
            })
            .collect();
        let tools_part = fields.get(read::tools_field(api)).copied();
        let tools = tools_part.map(|part| self.tools(part));
        let list = conversation.map(|(_, items)| {
            (items.into_iter())
                .map(|part| self.part(api, None, part, record))
                .collect()
        });
        let step = &mut self.steps[index];
        (step.tools, step.tools_part, step.fields) = (tools, tools_part, fields);
        if let Some(list) = list {
            (step.list, step.form) = (list, Some(Form::Apart));
        } else {
            step.notes.push(
                "the record kept its body whole, so its conversation is compared with no other"
                    .to_owned(),
            );
            step.form = Some(Form::Whole);
        }
    }

    /// Places step `index`, whose request was read, in the forest, after every step placed
    /// before it, and reads what it added; a step above whose call it pairs first is changed at
    /// `byte`, if given.
    fn derive(&mut self, index: usize, byte: Option<u64>) {
        let request = self.steps[index].request;
        self.derived = self.derived.max(Some(request));
        match self.steps[index].form {
            None => return,
            Some(Form::Whole) => {
                self.steps[index].thread = Some(request);
                self.add(index, 0, byte);
                return;
            }
            Some(Form::Apart) => {}
        }
        let list = std::mem::take(&mut self.steps[index].list);
        // The deepest node above the list's own that holds a step holds its parent.
        let (mut node, mut above) = (0, None);
        for (depth, &part) in list.iter().enumerate() {
            if !self.nodes[node].is_empty() {
                above = Some((node, depth));
            }
            let nodes = &mut self.nodes;
            node = *self.edges.entry((node, part)).or_insert_with(|| {
                nodes.push(Vec::new());
                nodes.len() - 1
            });
        }
        let parent = above.and_then(|(above, depth)| {
            let held = &self.nodes[above];
            let continued = held.iter().rev().copied();
            (continued
                .clone()
                .find(|&other| self.continues(list[depth], other)))
            .or_else(|| held.last().copied())
        });
        let same_as = self.nodes[node].last().copied();
        self.nodes[node].push(index);
        let thread = (parent.or(same_as))
            .and_then(|other| self.steps[other].thread)
            .unwrap_or(request);
        let from = parent.map_or(0, |parent| self.steps[parent].list.len());
        let step = &mut self.steps[index];
        step.list = list;
        (step.parent, step.same_as, step.thread) = (parent, same_as, Some(thread));
        self.add(index, from, byte);
    }

    /// Whether `part`, the first item that a request sent past step `other`'s list, continues
    /// `other`'s answer: each of its entries sends back an entry of it, or is the output of one of
    /// its calls. Each, as samples of one list may answer with the same text and other calls.
    fn continues(&self, part: u32, other: usize) -> bool {
        let Ok(entries) = &self.parts[part as usize].entries else {
            return false;
        };
        let answer = &self.steps[other].answer;
        !entries.is_empty()
            && entries.iter().all(|entry| {
                answer.iter().any(|answered| {
                echoed(entry, answered)
                    || matches!(
                        (entry, answered),
                        (Item::Output { call_id: Some(id), .. }, Item::Call { id: Some(call), .. })
                            if id == call
                    )
            })
            })
    }

    /// Places every step read again, in the order of their requests, once one was read after a
    /// later one was placed; each step whose place or pairs that changes is changed at `byte`,
    /// the line that `since` sends it again from.
    fn derive_again(&mut self, byte: Option<u64>) {
        let before = self.all_derived();
        self.nodes = vec![Vec::new()];
        self.edges.clear();
        self.pairs.clear();
        self.derived = None;
        for step in &mut self.steps {
            (step.parent, step.thread, step.same_as, step.turn) = (None, None, None, None);
            step.added.clear();
        }
        let placed: Vec<usize> = (self.at.values().copied())
            .filter(|&index| self.steps[index].form.is_some())
            .collect();
        // Only what differs from the forest before is changed, below.
        for index in placed {
            self.derive(index, None);
        }
        let after = self.all_derived();
        let mut left = BTreeSet::new();
        for ((step, before), after) in self.steps.iter_mut().zip(before).zip(after) {
            if before != after {
                step.changed = byte;
            }
            if before.thread != after.thread {
                left.extend(before.thread);
            }
        }
        // A thread that a step left is sent again with its root, so its span is said anew.
        for root in left {
            if let Some(&index) = self.at.get(&root) {
                self.steps[index].changed = byte;
            }
        }
    }

    /// Where each step is in the forest, and what it pairs, by its index.
    fn all_derived(&self) -> Vec<Derived> {
        let mut pairs: BTreeMap<u64, Vec<(String, String)>> = BTreeMap::new();
        for (at, other) in &self.pairs {
            let request = at
                .split('/')
                .next()
                .and_then(|request| request.parse().ok());
            if let Some(request) = request {
                pairs
                    .entry(request)
                    .or_default()
                    .push((at.clone(), other.clone()));
            }
        }
        let request_of = |other: Option<usize>| other.map(|other| self.steps[other].request);
        (self.steps.iter())
            .map(|step| Derived {
                thread: step.thread,
                parent: request_of(step.parent),
                turn: step.turn,
                same_as: request_of(step.same_as),
                added: (step.added.iter())
                    .map(|(address, _, _)| address.clone())
                    .collect(),
                pairs: pairs.remove(&step.request).unwrap_or_default(),
            })
            .collect()
    }

    /// Reads the entries that step `index` added past the first `from` items of its list, and
    /// the fields that differ from its parent's, less those that send back its parent's answer;
    /// pairs each output with its call, as [`Self::pair`] does at `byte`, and finds its turn.
    fn add(&mut self, index: usize, from: usize, byte: Option<u64>) {
        let step = &self.steps[index];
        let parent = step.parent.map(|parent| &self.steps[parent]);
        let echoes = parent.map_or(&[][..], |parent| &parent.answer[..]);
        let mut added = Vec::new();
        for (field, &part) in &step.fields {
            if parent.is_some_and(|parent| parent.fields.get(field) == Some(&part)) {
                continue;
            }
            let count = self.parts[part as usize]
                .entries
                .as_ref()
                .map_or(1, Vec::len);
            for at in 0..count {
                added.push((Address::Head(field.clone(), at), part, at));
            }
        }
        for (item, &part) in step.list.iter().enumerate().skip(from) {
            match &self.parts[part as usize].entries {
                Ok(entries) => {
                    for (at, entry) in entries.iter().enumerate() {
                        if !echoes.iter().any(|answer| echoed(entry, answer)) {
                            added.push((Address::In(item, at), part, at));
                        }
                    }
                }
                Err(_) => added.push((Address::In(item, 0), part, 0)),
            }
        }
        let user = added.iter().any(|(_, part, at)| {
            let entries = self.parts[*part as usize].entries.as_ref();
            matches!(
                entries.map(|entries| entries.get(*at)),
                Ok(Some(Item::User { .. }))
            )
        });
        // A retry or a repeat is of its earlier request's turn.
        let turn = match (step.same_as, parent) {
            (Some(same), _) => self.steps[same].turn,
            (None, Some(parent)) if !user => parent.turn,
            (None, _) => Some(step.request),
        };
        self.pair(index, &added, byte);
        let step = &mut self.steps[index];
        (step.added, step.turn) = (added, turn);
    }

    /// Pairs each output among `added`, the entries step `index` added, with its call, in the
    /// answer of a step above it; a call is paired with its first output, and its step is then
    /// changed at `byte`, if given.
    fn pair(&mut self, index: usize, added: &[(Address, u32, usize)], byte: Option<u64>) {
        let step = &self.steps[index];
        let mut pairs = Vec::new();
        for (address, part, at) in added {
            let entries = self.parts[*part as usize].entries.as_ref();
            let Ok(Some(Item::Output {
                call_id: Some(id), ..
            })) = entries.map(|entries| entries.get(*at))
            else {
                continue;
            };
            let mut up = step.parent;
            for _ in 0..CALL_LOOKUP_STEPS {
                let Some(at) = up else {
                    break;
                };
                let above = &self.steps[at];
                let call = above.answer.iter().position(
                    |answer| matches!(answer, Item::Call { id: Some(call), .. } if call == id),
                );
                if let Some(call) = call {
                    pairs.push((
                        at,
                        Address::Out(call).of(above.request),
                        address.of(step.request),
                    ));
                    break;
                }
                up = above.parent;
            }
        }
        for (above, call, output) in pairs {
            self.pairs.insert(output.clone(), call.clone());
            if let Entry::Vacant(first) = self.pairs.entry(call) {
                first.insert(output);
                if byte.is_some() {
                    self.steps[above].changed = byte;
                }
            }
        }
    }

    /// The index of `part`, of `field` if it is one, read once, as `api` writes it.
    fn part(&mut self, api: Api, field: Option<&str>, part: KeptPart, record: &Record) -> u32 {
        let key = match &part {
            KeptPart::Kept(digest) => digest.to_string(),
            KeptPart::Sent(value) => value.to_string(),
        };
        let key = (field.map(str::to_owned), key);
        if let Some(&index) = self.keys.get(&key) {
            return index;
        }
        let (digest, value) = match part {
            KeptPart::Kept(digest) => {
                let value = (record.kept_item(&digest)).and_then(|bytes| {
                    serde_json::from_slice::<Value>(&bytes)
                        .map_err(|error| format!("item {}: {error}", digest.hex()))
                });
                (Some(digest.hex().to_owned()), value)
            }
            KeptPart::Sent(value) => (None, Ok(value)),
        };
        let is_tools = field == Some(read::tools_field(api));
        let tools = is_tools.then(|| match &value {
            Ok(tools) => read::config(api, &json!({ "tools": tools })).tools,
            Err(_) => Vec::new(),
        });
        let entries = value.map(|value| match field {
            None => read::item(api, &value),
            // Its tools are read as a list of tools ([`Self::tools`]), and are no item.
            Some(_) if is_tools => Vec::new(),
            Some(field) => {
                let mut body = Map::new();
                body.insert(field.to_owned(), value);
                read::items(api, &Value::Object(body))
            }
        });
        self.parts.push(Part {
            digest,
            tools,
            entries,
        });
        let index = u32::try_from(self.parts.len() - 1).unwrap_or(u32::MAX);
        self.keys.insert(key, index);
        index
    }

    /// The index of the list of tools that `part` is.
    fn tools(&mut self, part: u32) -> usize {
        if let Some(&index) = self.tools_of.get(&part) {
            return index;
        }
        let Part { digest, tools, .. } = &self.parts[part as usize];
        self.tools.push(Tools {
            digest: digest.clone(),
            names: tools.clone().unwrap_or_default(),
        });
        let index = self.tools.len() - 1;
        self.tools_of.insert(part, index);
        index
    }
}

/// Whether `entry`, which a request sent, sends back `answer`, an entry of its parent's answer:
/// the same text of the same kind, or the same call, by its id. A request's reasoning is matched
/// by its text, as a client may send none of it back.
fn echoed(entry: &Item, answer: &Item) -> bool {
    match (entry, answer) {
        (Item::Assistant { text }, Item::Assistant { text: answered })
        | (Item::Reasoning { text }, Item::Reasoning { text: answered }) => text == answered,
        (
            Item::Call { id: Some(id), .. },
            Item::Call {
                id: Some(answered), ..
            },
        ) => id == answered,
        (
            Item::Call {
                id: None,
                name,
                arguments,
            },
            Item::Call {
                id: None,
                name: answered,
                arguments: passed,
            },
        ) => name == answered && arguments == passed,
        _ => false,
    }
}

impl Step {
    fn new(request: u64) -> Self {
        Self {
            request,
            placed: false,
            form: None,
            api: None,
            list: Vec::new(),
            fields: BTreeMap::new(),
            tools_part: None,
            parent: None,
            thread: None,
            same_as: None,
            changed: None,
            turn: None,
            added: Vec::new(),
            answer: Vec::new(),
            stop: None,
            whole: false,
            tools: None,
            notes: Vec::new(),
        }
    }
}
