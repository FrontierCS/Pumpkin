// A trace's ledger: its rows laid out from the steps and the entries read so far, under the
// folds and filters of the address, and each row drawn as one line. A thread's header
// (when the caller has several), a step's header, then the step's added entries and its answer;
// a step whose entries aren't read yet holds a row for each entry the server counts, which the
// entries replace when they come.

import { clock, h, plural, span, utc } from "./h.js";
import { IN_FLIGHT, Steps } from "./steps.js";

/**
 * @typedef {import("./steps.js").Entry} Entry
 * @typedef {{ k: "thread", root: number }
 *   | { k: "step", s: number }
 *   | { k: "entry", s: number, e: Entry }
 *   | { k: "calls", s: number, entries: Entry[] }
 *   | { k: "wait", s: number }} Row
 * @typedef {object} Shape what of the trace the address shows
 * @property {"" | "calls" | "steps" | "threads"} fold the level folded everywhere
 * @property {Set<number>} threads the threads whose fold is the other way, by root
 * @property {Set<number>} steps the steps whose fold is the other way, by request
 * @property {number | null} thread the one thread shown, by root
 * @property {string} kind the one kind of entry shown
 * @typedef {object} Marks what a search found
 * @property {Set<string>} at the entries' addresses
 * @property {Map<number, number>} steps how many in each step, by request
 */

/** @param {Shape} shape @param {number} root */
export const threadFolded = (shape, root) => (shape.fold === "threads") !== shape.threads.has(root);

/** @param {Shape} shape @param {number} request */
export const stepFolded = (shape, request) => (shape.fold === "threads" || shape.fold === "steps") !== shape.steps.has(request);

// The rows a step holds before its entries are read: those it added and those of its answer, as
// the server counts them; of one kind, its calls.
/** @param {Steps} steps @param {number} s @param {string} kind */
function expected(steps, s, kind) {
  const c = steps.cols;
  if (kind) return kind === "call" ? (c.calls[s] || []).length : 0;
  const n = (c.added_entries[s] ?? 0) + (c.answers[s] ?? 0);
  // A step in flight holds a row for the answer it is giving.
  return steps.inFlight(s) ? Math.max(1, n) : n;
}

// The rows of `steps` under `shape`; `entriesOf(s)` gives the entries read of the step at row
// `s`, of the kind shown, or null if they are not read yet.
/** @param {Steps} steps @param {(s: number) => Entry[] | null} entriesOf @param {Shape} shape @returns {Row[]} */
export function layout(steps, entriesOf, shape) {
  /** @type {Row[]} */
  const rows = [];
  const several = steps.reply.threads > 1 || (steps.threads.size > 1 && !shape.thread);
  for (const [root, of] of steps.threads) {
    if (shape.thread !== null && root !== shape.thread) continue;
    if (several) {
      rows.push({ k: "thread", root });
      if (threadFolded(shape, root)) continue;
    }
    for (const s of of) {
      rows.push({ k: "step", s });
      if (stepFolded(shape, steps.cols.request[s])) continue;
      const entries = entriesOf(s);
      if (entries === null) {
        for (let n = expected(steps, s, shape.kind); n > 0; n--) rows.push({ k: "wait", s });
        continue;
      }
      for (let at = 0; at < entries.length; at++) {
        const e = entries[at];
        if (shape.fold === "calls" && (e.kind === "call" || e.kind === "output")) {
          let end = at;
          while (end < entries.length && entries[end].kind === e.kind) end++;
          rows.push({ k: "calls", s, entries: entries.slice(at, end) });
          at = end - 1;
        } else rows.push({ k: "entry", s, e });
      }
    }
  }
  return rows;
}

// The address of what a row selects: a thread `t<root>`, a step its request, an entry its own;
// null for a row that only stands for others.
/** @param {Steps} steps @param {Row} row */
export function keyOf(steps, row) {
  if (row.k === "thread") return "t" + row.root;
  if (row.k === "step") return String(steps.cols.request[row.s]);
  if (row.k === "entry") return row.e.at;
  return null;
}

// The first line of `text` that holds more than space, cut to what a row can show.
/** @param {string} text */
export function firstLine(text) {
  for (const line of text.split("\n", 40)) {
    const kept = line.trim();
    if (kept) return kept.length > 400 ? kept.slice(0, 400) + "…" : kept;
  }
  return "";
}

/**
 * @typedef {object} Context what drawing a row needs besides the row
 * @property {Steps} steps
 * @property {Shape} shape
 * @property {Map<string, Entry>} byAt every entry read, by its address
 * @property {Marks | null} marks
 * @property {number | null} started the run's start, a Unix time in seconds
 * @property {(row: Row) => void} toggle folds or opens the row's thread, step or calls
 */

// How many of the steps at rows `of` are retries or repeats, as `column` says, after a `·`.
/** @param {number[]} of @param {Array<number | null>} column @param {string} one @param {string} many */
function again(of, column, one, many) {
  const n = of.filter((s) => column[s] !== null).length;
  return n ? " · " + n + " " + (n === 1 ? one : many) : "";
}

/** @param {number} n */
export const matches = (n) => n.toLocaleString("en-US") + (n === 1 ? " match" : " matches");

/** @param {boolean} folded @param {() => void} toggle @param {string} what */
function caret(folded, toggle, what) {
  return h("span", { class: "caret", title: (folded ? "Open " : "Fold ") + what, onclick: (/** @type {Event} */ event) => (event.stopPropagation(), toggle()) }, folded ? "▸" : "▾");
}

// A mark on a step whose system prompt or tool list differs from its parent's.
/** @param {string[] | null} parts */
function differs(parts) {
  if (!parts || !parts.length) return null;
  return h("span", { class: "differs", title: "Its " + parts.join(" and ") + " differ" + (parts.length > 1 ? "" : "s") + " from its parent's" }, "≠ " + parts.join(" "));
}

/** @param {Row} row @param {Context} ctx @returns {HTMLElement} */
export function drawRow(row, ctx) {
  const { steps, shape } = ctx;
  const c = steps.cols;
  if (row.k === "thread") {
    const of = steps.threads.get(row.root) || [];
    const calls = of.reduce((sum, s) => sum + (c.calls[s] || []).length, 0);
    const from = of.length ? steps.times(of[0]).start.value : null;
    const lasted = steps.spans.get(row.root) ?? null;
    const folded = threadFolded(shape, row.root);
    const said = plural(of.length, "step") + " · " + plural(calls, "tool call") + again(of, c.retry_of, "retry", "retries") + again(of, c.repeat_of, "repeat", "repeats");
    const hits = ctx.marks ? of.reduce((sum, s) => sum + (ctx.marks?.steps.get(c.request[s]) || 0), 0) : 0;
    return h(
      "div",
      { class: "lrow thread" + (hits ? " hit" : "") },
      caret(folded, () => ctx.toggle(row), "this thread"),
      h("span", { class: "name" }, row.root === IN_FLIGHT ? "In flight" : "Thread " + row.root),
      h("span", { class: "said", title: said }, said),
      h("span", { class: "t", title: "Its first step's start, and its span from its first start to its last end" }, from === null && lasted === null ? "-" : clock(from) + "  " + (lasted === null ? "-" : span(lasted / 1e9))),
      hits ? h("span", { class: "hits" }, matches(hits)) : null,
    );
  }
  if (row.k === "step") {
    const s = row.s;
    const request = c.request[s];
    const { at, of } = steps.place(s);
    const times = steps.times(s);
    const when = times.end.value ?? times.start.value;
    const again = steps.again(s);
    const hits = ctx.marks?.steps.get(request) || 0;
    const turn = c.turn[s] === request && c.parent[s] !== null;
    // A step that continues another than the one above it in its thread begins a branch; a retry
    // or a repeat, which says what it sends again, needs no mark.
    const parent = c.parent[s];
    const above = at > 1 ? c.request[/** @type {number[]} */ (steps.threads.get(c.thread[s] ?? IN_FLIGHT))[at - 2]] : null;
    const branch = parent !== null && parent !== above && c.retry_of[s] === null && c.repeat_of[s] === null;
    const tone = steps.tone(s);
    return h(
      "div",
      { class: "lrow step " + tone + (hits ? " hit" : "") },
      caret(stepFolded(shape, request), () => ctx.toggle(row), "this step"),
      h("span", { class: "mark " + tone, title: tone === "live" ? "In flight" : "Status " + (c.status[s] ?? "-") + ", " + (c.answer[s] ?? "no answer") + (tone === "failed" ? ", " + steps.why(s) : "") }),
      h("span", { class: "req", title: "Request " + request + " of the run" }, "#" + request),
      h("span", { class: "pos", title: c.thread[s] === null ? "Placed in a thread once answered" : "Step " + at + " of the " + of + " of thread " + c.thread[s] }, c.thread[s] === null ? "-" : at + "/" + of),
      h("span", { class: "t", title: when === null ? times.end.why : utc(ctx.started, when) + (times.end.value === null ? " (its start)" : " (its answer)") }, clock(when)),
      h("span", { class: "dur", title: times.model.value === null ? "Model time unknown: " + times.model.why : "Model time, from its start to its answer" }, span(times.model.value)),
      ...steps.tokens(s).map((cell, at) => h("span", { class: "tk tk" + at }, cell)),
      turn ? h("span", { class: "turn", title: "This step begins a turn: it adds a user entry" }, "Turn") : null,
      branch ? h("span", { class: "branch", title: "It continues request " + parent + ", not the step above it: a branch of its thread" }, "↳ #" + parent) : null,
      differs(c.differs[s]),
      again ? h("span", { class: "again", title: again }, again.replace("The same conversation as", "Same as").replace(/request /g, "#")) : null,
      h("span", { class: "calls" }, tone === "live" ? "Answering…" : steps.calls(s)),
      c.notes[s] ? h("span", { class: "note", title: (c.notes[s] || []).join("\n") }, "Note") : null,
      hits ? h("span", { class: "hits" }, matches(hits)) : null,
    );
  }
  if (row.k === "wait") return h("div", { class: "lrow wait" }, h("span", { class: "kind" }, ""), h("span", { class: "text" }, "Reading…"));
  if (row.k === "calls") {
    const calls = row.entries[0].kind === "call";
    /** @type {Map<string, number>} */
    const names = new Map();
    for (const e of row.entries) {
      const name = (calls ? e.name : e.call && ctx.byAt.get(e.call)?.name) || "?";
      names.set(name, (names.get(name) || 0) + 1);
    }
    const said = [...names].map(([name, n]) => (n > 1 ? name + "×" + n : name)).join(" ");
    return h("div", { class: "lrow entry folded", title: "Open the calls (the fold of calls)" }, h("span", { class: "kind k-" + row.entries[0].kind }, row.entries[0].kind), h("span", { class: "text" }, plural(row.entries.length, calls ? "tool call" : "tool output") + " · " + said));
  }
  const e = row.e;
  const hit = ctx.marks?.at.has(e.at);
  /** @type {Array<Node | string>} */
  const text = [];
  if (e.kind === "call") {
    const output = e.output ? ctx.byAt.get(e.output) : undefined;
    text.push(h("span", { class: "name" }, e.name || "?"), h("span", { class: "args" }, firstLine(e.text)));
    text.push(h("span", { class: "arrow" }, "→"), h("span", { class: "result", title: output ? null : e.output ? "Its output, " + e.output + ", isn't read yet" : "No later step sends its output" }, output ? firstLine(output.text) : e.output ? "…" : "No output yet"));
  } else if (e.kind === "output") {
    const call = e.call ? ctx.byAt.get(e.call) : undefined;
    text.push(h("span", { class: "arrow" }, "← "), h("span", { class: "name" }, call?.name || (e.call ? "Call " + e.call : "No call found")), h("span", { class: "line" }, firstLine(e.text)));
  } else text.push(h("span", { class: "line" }, firstLine(e.text)));
  return h(
    "div",
    { class: "lrow entry " + e.side + (hit ? " hit" : ""), title: e.text.length > 2000 ? e.text.slice(0, 2000) + "…" : e.text },
    h("span", { class: "kind k-" + e.kind }, e.kind),
    h("span", { class: "text" }, ...text),
  );
}
