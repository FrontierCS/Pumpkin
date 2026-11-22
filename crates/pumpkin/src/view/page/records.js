// What a record of a run's log says, as the run's log and a trace's show it: the lanes it
// concerns, its kind and words, polls in a row folded, and its row in a list.

import { bytes, clock, count, h, plural, utc } from "./h.js";

/**
 * @typedef {object} LogRecord a record of the run's log, its strings cut, a note's or an event's
 * @property {number} byte where it starts in the log
 * @property {number | null} when seconds since the run started
 * @property {number} [seq]
 * @property {string} [note]
 * @property {string} [unread] the line, if it is no JSON object
 * @property {{ kind: string, [field: string]: any }} [event]
 * @property {Array<{ kind: string, [field: string]: any }>} [actions]
 * @typedef {{ records: LogRecord[], next: number, end: number }} Window `/log`
 * @typedef {{ fold: LogRecord[] }} Fold
 * @typedef {LogRecord | Fold} Item
 */

// Where the log is read: `cursor`, the byte read to.
/** @typedef {{ cursor: number }} Reading */

// Moves `read` past `window`, and gives its records past those read before.
/** @param {Window} window @param {Reading} read */
export function pastWindow(window, read) {
  const records = window.records.filter((record) => record.byte >= read.cursor);
  read.cursor = window.next;
  return records;
}

// The lanes that a log record concerns, by their names: `agent`, `job <n>`, `lease <n>`.
/** @param {LogRecord} record */
export function lanesOf(record) {
  const lanes = new Set();
  const event = /** @type {Record<string, any>} */ (record.event || {});
  const caller = event.caller;
  if (typeof caller === "string") lanes.add(caller.replace("-", " "));
  if (event.shared !== undefined) lanes.add("shared " + event.shared);
  if (event.job !== undefined) lanes.add("job " + event.job);
  if (event.lease !== undefined) lanes.add("lease " + event.lease);
  if (event.sandbox) lanes.add(String(event.sandbox).replace("-", " "));
  const request = event.request || {};
  if (request.lease !== undefined) lanes.add("lease " + request.lease);
  if (request.job !== undefined) lanes.add("job " + request.job);
  for (const action of record.actions || []) {
    if (action.shared !== undefined) lanes.add("shared " + action.shared);
    if (action.lease !== undefined) lanes.add("lease " + action.lease);
    if (action.job !== undefined) lanes.add("job " + action.job);
    if (action.caller) lanes.add(String(action.caller).replace("-", " "));
    if (action.holder) lanes.add(String(action.holder).replace("-", " "));
  }
  return lanes;
}

// What a model answer's `failed` says, after its status: the answer stated that it failed.
/** @param {{ code?: string } | null | undefined} failed */
const failure = (failed) => (failed ? ", failed: " + (failed.code ?? "no code given") : "");

// A log record in a line: its kind, what happened and what the core did about it, and whether it
// failed or was refused.
/** @param {LogRecord} record @returns {[string, string, string]} */
function said(record) {
  if (record.unread !== undefined) return ["unread", record.unread, "failed"];
  if (record.note !== undefined) return ["note", record.note, ""];
  const event = /** @type {Record<string, any>} */ (record.event || {});
  const replies = (record.actions || []).filter((action) => action.kind === "reply");
  const acts = (record.actions || []).filter((action) => action.kind !== "reply").map((action) => action.kind);
  let text = "";
  switch (event.kind) {
    case "op_arrived": {
      const { kind, ...rest } = event.request || {};
      const args = Object.entries(rest)
        .filter(([, value]) => value !== null)
        .map(([name, value]) => name + "=" + (typeof value === "object" ? JSON.stringify(value) : value));
      text = kind + " " + args.join(" ");
      const reply = replies[0] && replies[0].reply;
      if (reply) text += "  → " + reply.kind + (reply.kind === "refused" ? ": " + reply.message : "");
      return [kind, text, reply && reply.kind === "refused" ? "refused" : ""];
    }
    case "command_ended":
      text = (event.lease === undefined ? "job " + event.job : "lease " + event.lease) + " command " + event.command + ": " + JSON.stringify(event.end);
      break;
    case "transfer_ended": {
      const moved = event.end && event.end.moved;
      text = "lease " + event.lease + " transfer " + event.transfer + ": " + (moved ? plural(moved.entries, "entry", "entries") + ", " + bytes(moved.bytes) : JSON.stringify(event.end));
      return [event.kind, text, moved ? "" : "failed"];
    }
    case "model_answered": {
      const usage = event.usage ? " · " + count(event.usage.input_tokens, "tok") + " in, " + count(event.usage.output_tokens, "tok") + " out" : "";
      text = "request " + event.request + " of " + (event.caller || "?") + ": " + event.answer.kind + " " + (event.upstream_status ?? "") + failure(event.failed) + usage;
      return [event.kind, text, event.failed ? "failed" : event.answer.kind === "forwarded" ? "" : "refused"];
    }
    case "call_returned": {
      const over = event.model_requests_over_budget ? " · " + plural(event.model_requests_over_budget, "model request") + " over budget" : "";
      text = "job " + event.job + ": " + event.returned.map((shown) => shown.name + "=" + JSON.stringify(shown.value)).join(" ") + over;
      break;
    }
    case "run_started":
      text = "hard end at " + clock(Number(event.hard_end_ns) / 1e9);
      break;
    default: {
      const { kind, ...rest } = event;
      text = Object.entries(rest)
        .map(([name, value]) => name + "=" + JSON.stringify(value))
        .join(" ");
    }
  }
  if (acts.length) text += "  ⇒ " + acts.join(", ");
  return [event.kind, text, event.kind && event.kind.endsWith("failed") ? "failed" : ""];
}

// `said(record)`, worked out once for each record.
const saidOnce = new WeakMap();
/** @param {LogRecord} record @returns {[string, string, string]} */
export function saidOf(record) {
  if (!saidOnce.has(record)) saidOnce.set(record, said(record));
  return saidOnce.get(record);
}

// The ops that a caller sends again and again to wait for something.
const POLLS = ["status", "lease_status"];
// The fewest polls in a row that the log folds into one row.
const FOLD_MIN = 3;

// `records` with each run of FOLD_MIN or more of the same poll in a row folded into one
// `{ fold: [...] }`, which is followed by its records if `open` holds its first record's byte.
/** @param {LogRecord[]} records @param {Set<number>} open @returns {Item[]} */
export function folded(records, open) {
  /** @type {Item[]} */
  const out = [];
  for (let index = 0; index < records.length; ) {
    const [kind, text] = saidOf(records[index]);
    let end = index + 1;
    if (POLLS.includes(kind) && records[index].event?.kind === "op_arrived") {
      while (end < records.length && saidOf(records[end])[1] === text) end++;
    }
    if (end - index >= FOLD_MIN) {
      const fold = { fold: records.slice(index, end) };
      out.push(fold);
      if (open.has(records[index].byte)) out.push(...fold.fold);
    } else {
      out.push(...records.slice(index, end));
    }
    index = end;
  }
  return out;
}

// The choice of the kinds of records in `state`, each with its count, `kind` chosen; drawn again
// only when the kinds or their counts change, so that an open choice stays open.
/** @param {HTMLSelectElement} select @param {{ records: LogRecord[], kinds: string }} state @param {string | null} kind */
export function drawKinds(select, state, kind) {
  const counts = new Map();
  for (const record of state.records) {
    const [what] = saidOf(record);
    counts.set(what, (counts.get(what) || 0) + 1);
  }
  const names = [...counts.keys()].sort();
  const key = names.map((name) => name + counts.get(name)).join(" ") + "/" + kind;
  if (key === state.kinds) return;
  state.kinds = key;
  select.replaceChildren(h("option", { value: "" }, "All kinds"), ...names.map((name) => h("option", { value: name }, name + " (" + counts.get(name).toLocaleString("en-US") + ")")));
  select.value = kind || "";
}

// Whether `record` is of kind `kind`, if one is given, and holds `needle`, in lower case, in its
// kind or words.
/** @param {LogRecord} record @param {string | null} kind @param {string} needle */
export function holds(record, kind, needle) {
  const [what, text] = saidOf(record);
  return (!kind || what === kind) && (!needle || (what + " " + text).toLowerCase().includes(needle));
}

// The row of `item`, a record or a fold of polls, which is open if `open` holds its first
// record's byte; `started` is the run's start, a Unix time in seconds, for a time's hover.
/** @param {Item} item @param {Set<number>} open @param {number | null | undefined} started */
export function recordRow(item, open, started) {
  const fold = "fold" in item ? item.fold : null;
  const first = fold ? fold[0] : /** @type {LogRecord} */ (item);
  const [kind, text, flag] = saidOf(first);
  const who = [...lanesOf(first)].join(" ");
  const opened = fold && open.has(first.byte);
  const times = fold ? (opened ? "▾ " : "▸ ") + "×" + fold.length + " to " + clock(fold[fold.length - 1].when) : null;
  return h(
    "div",
    { class: (flag || "") + (fold ? " fold" : ""), title: fold ? "Show or hide these records" : null },
    h("span", { class: "t", title: utc(started, first.when) }, clock(first.when)),
    h("span", { class: "who" }, who),
    h("span", { class: "kind" }, kind),
    h("span", { class: "text" }, text),
    times && h("span", { class: "times" }, times),
  );
}
