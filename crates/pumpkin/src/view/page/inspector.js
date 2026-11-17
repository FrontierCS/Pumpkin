// A trace's inspector: what is selected in the ledger, a step, an entry, a call or a thread, on
// tabs. Text is shown as it was sent, its whitespace kept; a text the server cut is fetched
// whole on asking; the bytes the record keeps are shown as kept.

import { Refused, STATIC, bytes, clock, count, fill, get, getText, h, plural, span, utc } from "./h.js";
import { diffTab } from "./diff.js";
import { firstLine } from "./ledger.js";
import { Steps } from "./steps.js";

/**
 * @typedef {import("./steps.js").Entry} Entry
 * @typedef {import("./steps.js").Timed} Timed
 * @typedef {import("./steps.js").StepStats} StepStats
 * @typedef {import("./h.js").Child} Child
 * @typedef {object} Exchange `exchange/<n>`
 * @property {string | null} request_id
 * @property {string | null} api
 * @property {string | null} upstream
 * @property {Answer | null} answer null in flight
 * @property {Record<string, any>} usage
 * @property {{ request?: Record<string, any>, response?: Record<string, any> }} kept
 * @property {Record<string, any>} config
 * @property {StepStats | null} stats the step's row of `stats`
 * @typedef {{ kind: "forwarded" | "cut" | "abandoned" } | { kind: "barred", bar: "rules" | "ended" | "budget", status: number, message: string } | { kind: "refused", status: number, message: string }} Answer
 * @typedef {object} Context
 * @property {Steps} steps
 * @property {string} base the run's address in the server's API
 * @property {string} caller
 * @property {string} api the trace's address in the server's API
 * @property {Map<string, Entry>} byAt every entry read, by its address
 * @property {(s: number) => Entry[] | null} entriesOf the entries read of the step at a row, every kind
 * @property {number | null} started the run's start, a Unix time in seconds
 * @property {string} needle the search's text, as typed
 * @property {string | null} tab the tab the address names
 * @property {(tab: string) => void} setTab
 * @property {(key: string) => void} go selects what `key` addresses
 * @typedef {{ k: "none" } | { k: "thread", root: number } | { k: "step", s: number } | { k: "entry", e: Entry }} Selected
 */

// What barred a request, by the bar that the route names.
const BARS = { rules: "the route's rules", ended: "the route's end", budget: "a spent model budget" };
// How much of a text the inspector shows before "show all", as the server cuts strings.
const SHOWN_BYTES = 8192;
// How much of a kept request or response the inspector shows at once.
const RAW_SHOWN = 256 * 1024;
// The mark the server puts at the end of a string it cut: `… [<n> bytes in all]`.
const CUT = /… \[(\d+) bytes in all\]$/;

/** @type {Map<string, Promise<Exchange>>} */
const exchanges = new Map();
// Forgets the exchanges read, as a log that started over names other requests.
export const forgetExchanges = () => exchanges.clear();
// The exchange of request `n`, read once.
/** @param {string} base @param {number} n */
function exchange(base, n) {
  const key = base + "/" + n;
  let read = exchanges.get(key);
  if (!read) {
    read = get(base + "/exchange/" + n);
    read.catch(() => exchanges.delete(key));
    exchanges.set(key, read);
  }
  return read;
}

// `text` with ASCII capitals in lower case, and no other letter changed, as the server's search
// folds them: the text keeps its length, so a place in it is a place in `text`.
/** @param {string} text */
export const fold = (text) => text.replace(/[A-Z]+/g, (letters) => letters.toLowerCase());

// `text` with each place that holds `needle` marked, as text nodes and `mark` elements, matched
// as the server's search matches.
/** @param {string} text @param {string} query @returns {Array<Node | string>} */
export function marked(text, query) {
  if (!query) return [text];
  const lower = fold(text);
  const needle = fold(query);
  /** @type {Array<Node | string>} */
  const out = [];
  let from = 0;
  for (let at = lower.indexOf(needle); at >= 0 && out.length < 2000; at = lower.indexOf(needle, at + needle.length)) {
    out.push(text.slice(from, at), h("mark", {}, text.slice(at, at + needle.length)));
    from = at + needle.length;
  }
  out.push(text.slice(from));
  return out;
}

/** @param {string} text */
const copy = (text) => h("button", { title: "Copy this to the clipboard", onclick: () => navigator.clipboard && navigator.clipboard.writeText(text) }, "Copy");

// A text as sent, in mono with its whitespace, cut where the server cut it, with a "show all"
// that reads the entry at `at` uncut.
/** @param {string} text @param {string} needle @param {string} api @param {string} at */
function sent(text, needle, api, at) {
  const cut = CUT.exec(text);
  const pre = h("pre", { class: "sent" }, marked(text, needle));
  if (!cut) return h("div", {}, h("p", { class: "tools" }, copy(text)), pre);
  const tools = h("p", { class: "tools" });
  const all = h("button", {
    title: "Read the whole text from the server",
    onclick: async () => {
      fill(tools, "Reading " + bytes(Number(cut[1])) + "…");
      try {
        /** @type {Entry} */
        const entry = await get(api + "/entry?at=" + encodeURIComponent(at));
        const full = entry.text;
        const still = entry.text_bytes !== undefined && entry.text_bytes > new TextEncoder().encode(full).length;
        fill(pre, marked(full, needle));
        fill(tools, copy(full), h("span", { class: "dim" }, still ? " the first " + bytes(new TextEncoder().encode(full).length) + " of " + bytes(entry.text_bytes ?? null) + " shown" : " " + bytes(Number(cut[1])) + " in all"));
      } catch (error) {
        fill(tools, h("span", { class: "error" }, "Reading it failed: " + (error instanceof Error ? error.message : String(error))));
      }
    },
  }, "Show all");
  fill(tools, all, h("span", { class: "dim" }, " cut at " + bytes(SHOWN_BYTES) + " of " + bytes(Number(cut[1]))));
  return h("div", {}, tools, pre);
}

// The items of a kept request read at once, at most.
const ITEMS_AT_ONCE = 8;

// Request `path` (`…/exchange/<n>/request`) rebuilt from the export's file of it and of its
// items, in the bytes that the server answers.
/** @param {string} path @returns {Promise<string>} */
async function rebuilt(path) {
  const base = path.slice(0, path.lastIndexOf("/exchange/"));
  /** @typedef {{ item: string } | { sent: any }} Part */
  /** @type {{ conversation: { field: string, items: Part[] } | null, fields: Array<{ name: string } & Part> }} */
  const kept = await get(path);
  const parts = [...kept.fields, ...(kept.conversation?.items ?? [])];
  const digests = [...new Set(parts.flatMap((part) => ("item" in part ? [part.item] : [])))];
  /** @type {Map<string, any>} */
  const items = new Map();
  for (let at = 0; at < digests.length; at += ITEMS_AT_ONCE) {
    await Promise.all(digests.slice(at, at + ITEMS_AT_ONCE).map(async (digest) => items.set(digest, await get(base + "/item/" + digest))));
  }
  const value = (/** @type {Part} */ part) => ("item" in part ? items.get(part.item) : part.sent);
  const fields = kept.fields.map((field) => [field.name, value(field)]);
  if (kept.conversation) fields.push([kept.conversation.field, kept.conversation.items.map(value)]);
  // In the order of their names, as the server writes an object, so that the bytes are its own.
  return JSON.stringify(Object.fromEntries(fields.sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))));
}

// Bytes the record keeps, read on asking, the first `RAW_SHOWN` of them shown as kept; in the
// static mode a request is rebuilt from its items.
/** @param {string} label @param {string} path @param {number | null | undefined} size */
function rawBytes(label, path, size) {
  const shown = h("div", {});
  const read = h("button", {
    title: "Read these bytes from the record",
    onclick: async () => {
      fill(shown, h("p", { class: "dim" }, "Reading…"));
      try {
        const text = await (STATIC && path.endsWith("/request") ? rebuilt(path) : getText(path));
        fill(shown, h("p", { class: "tools" }, copy(text), text.length > RAW_SHOWN ? h("span", { class: "dim" }, " the first " + bytes(RAW_SHOWN) + " shown") : null), h("pre", { class: "sent" }, text.length > RAW_SHOWN ? text.slice(0, RAW_SHOWN) : text));
      } catch (error) {
        fill(shown, h("p", { class: "error" }, "Reading it failed: " + (error instanceof Error ? error.message : String(error))));
      }
    },
  }, "Show");
  return h("div", { class: "raw" }, h("p", { class: "tools" }, h("span", { class: "name" }, label), h("span", { class: "num" }, bytes(size)), read), shown);
}

/** @param {Array<[string, string | Node | null, string?]>} pairs */
const facts = (pairs) =>
  h(
    "dl",
    {},
    pairs.flatMap(([name, value, title]) => (value === null ? [] : [h("dt", {}, name), h("dd", { title: title || null }, value)])),
  );

// A time or span, or `-` with why it is unknown.
/** @param {Timed} timed @param {(value: number) => string} show @param {string} [title] */
const timed = (timed, show, title) => (timed.value === null ? h("span", { class: "why", title: timed.why }, "- (" + timed.why + ")") : h("span", { title: title || null }, show(timed.value)));

// Draws the tabs of `tabs`, the one the address names if it has it, else the first.
/** @param {HTMLElement} node @param {Array<Node | null>} head @param {Array<[string, () => Child | Child[]]>} tabs @param {Context} ctx */
function tabbed(node, head, tabs, ctx) {
  const chosen = tabs.find(([name]) => name === ctx.tab) || tabs[0];
  const body = h("div", { class: "tab" });
  const bar = h(
    "div",
    { class: "tabs small", role: "tablist" },
    tabs.map(([name]) => h("button", { class: name === chosen[0] ? "on" : null, role: "tab", "aria-selected": name === chosen[0] ? "true" : "false", onclick: () => ctx.setTab(name) }, name)),
  );
  fill(body, chosen[1]());
  fill(node, ...head, bar, body);
}

/** @param {HTMLElement} node @param {Selected} selected @param {Context} ctx */
export function inspect(node, selected, ctx) {
  const { steps } = ctx;
  const c = steps.cols;
  if (selected.k === "none") {
    const total = (/** @type {Array<number | null>} */ column) => column.reduce((sum, n) => (sum ?? 0) + (n ?? 0), 0);
    return fill(
      node,
      h("h3", {}, "Trace of " + ctx.caller.replace("-", " ")),
      facts([
        ["Steps", plural(steps.length, "step")],
        ["Threads", plural(steps.reply.threads, "thread")],
        ["Tokens", count(total(c.input), "tok") + " in · " + count(total(c.cached), "tok") + " cached · " + count(total(c.output), "tok") + " out"],
        ["Tool calls", plural(c.calls.reduce((sum, calls) => sum + (calls || []).length, 0), "call")],
        ["Retries", c.retry_of.filter((r) => r !== null).length.toLocaleString("en-US")],
        ["Repeats", c.repeat_of.filter((r) => r !== null).length.toLocaleString("en-US")],
      ]),
      h("p", { class: "empty" }, "Select a row (j and k step through rows, [ and ] through steps). / searches; Enter and Shift-Enter go to the next and previous match; Esc clears."),
    );
  }
  if (selected.k === "thread") {
    const of = steps.threads.get(selected.root) || [];
    return fill(
      node,
      h("h3", {}, h("span", { class: "kind" }, "Thread"), " " + selected.root),
      facts([
        ["Steps", plural(of.length, "step")],
        ["Tool calls", plural(of.reduce((sum, s) => sum + (c.calls[s] || []).length, 0), "call")],
        ["Retries", String(of.filter((s) => c.retry_of[s] !== null).length)],
        ["Repeats", String(of.filter((s) => c.repeat_of[s] !== null).length)],
        ["Turns", String(new Set(of.map((s) => c.turn[s])).size)],
      ]),
      h("p", { class: "links" }, h("button", { onclick: () => ctx.go(String(c.request[of[0]])) }, "Go to its first step")),
    );
  }
  if (selected.k === "step") return inspectStep(node, selected.s, ctx);
  return inspectEntry(node, selected.e, ctx);
}

/** @param {HTMLElement} node @param {number} s @param {Context} ctx */
function inspectStep(node, s, ctx) {
  const { steps } = ctx;
  const c = steps.cols;
  const n = c.request[s];
  const { at, of } = steps.place(s);
  const t = steps.times(s);
  const at0 = (/** @type {number} */ value) => clock(value);
  const held = h("div", {}, h("p", { class: "dim" }, "Reading the exchange…"));
  const config = h("div", {}, h("p", { class: "dim" }, "Reading the exchange…"));
  const measured = h("div", {}, h("p", { class: "dim" }, "Reading the exchange…"));
  exchange(ctx.base, n).then(
    (ex) => {
      const barred = ex.answer?.kind === "barred" ? ex.answer : null;
      fill(
        held,
        facts([
          ["Barred by", barred ? BARS[barred.bar] : null],
          ["The route said", barred ? barred.message : null],
          ["Request ID", ex.request_id ?? "-"],
          ["API", ex.api ?? "-"],
          ["Upstream", ex.upstream ?? "-"],
        ]),
      );
      if (ex.stats) {
        const d = steps.measured(s, ex.stats);
        fill(
          measured,
          facts([
            ["Queue", timed(d.queue, (v) => span(v) + " (" + d.queueIs + ")")],
            ["First byte", timed(d.firstByte, span)],
            ["TTFT", timed(d.ttft, span)],
            ["Decode", timed(d.decode, span)],
            ["Model time", timed(d.model, span)],
            ["tok/s", timed(d.rate, (v) => Math.round(v) + " tok/s")],
          ]),
        );
      } else fill(measured, h("p", { class: "error" }, "The server measured no times of it"));
      fill(config, ex.config && !ex.config.error ? h("pre", { class: "sent" }, JSON.stringify(ex.config, null, 2)) : h("p", { class: "error" }, "The request can't be rebuilt: " + (ex.config ? ex.config.error : "no config")));
    },
    (error) => {
      const unpublished = STATIC && error instanceof Refused && error.status === 404;
      const said = unpublished ? h("p", { class: "empty" }, "Not published yet: the exchange is published once this step and every one before it is answered.") : h("p", { class: "error" }, "Reading the exchange failed: " + (error instanceof Error ? error.message : String(error)));
      fill(held, said);
      fill(config, said.cloneNode(true));
      fill(measured, said.cloneNode(true));
    },
  );
  const parent = c.parent[s];
  const again = steps.again(s);
  const items = () => {
    const added = c.added_entries[s];
    if (c.items[s] !== null) return c.items[s] + " sent, " + (c.added[s] ?? "-") + " added";
    return added === null ? "-" : plural(added, "entry", "entries") + " added, no count of the items it sent";
  };
  const link = (/** @type {number} */ to, /** @type {string} */ label) => h("button", { class: "mono", title: "Select request " + to, onclick: () => ctx.go(String(to)) }, label);
  const summary = () => [
    facts([
      ["Status", h("span", {}, h("span", { class: "mark " + steps.tone(s) }), (c.status[s] ?? "-") + " " + (c.answer[s] ?? (steps.inFlight(s) ? "in flight" : "-")))],
      ["Failed", c.failed[s] ? c.failed[s]?.code ?? "No code given" : null],
      ["Stop", c.stop[s] ?? "-"],
      ["Thread", c.thread[s] === null ? "- (placed once answered)" : c.thread[s] + ", step " + at + " of " + of],
      ["Parent", parent === null ? "-" : link(parent, "Request " + parent)],
      ["Turn", c.turn[s] === null ? "-" : "Begun by request " + c.turn[s]],
      ["Again", again],
      ["Differs", c.differs[s] && c.differs[s]?.length ? h("button", { title: "Show how, in the Diff tab", onclick: () => ctx.setTab("Diff") }, "Its " + (c.differs[s] || []).join(" and ") + " from its parent's") : null],
      ["Calls", steps.calls(s) || "-"],
      ["Items", items()],
      ["Notes", c.notes[s] ? (c.notes[s] || []).join("; ") : null],
    ]),
    held,
  ];
  const usage = () =>
    facts([
      ["Input", count(c.input[s], "tok")],
      ["Cached", count(c.cached[s], "tok") + (c.cache_hit[s] === null ? "" : " (" + Math.round(100 * /** @type {number} */ (c.cache_hit[s])) + "%)")],
      ["Cache write", count(c.cache_write[s], "tok")],
      ["Output", count(c.output[s], "tok")],
      ["Reasoning", count(c.reasoning[s], "tok")],
      ["Content", c.output[s] === null ? "-" : count(/** @type {number} */ (c.output[s]) - (c.reasoning[s] ?? 0), "tok")],
    ]);
  const timing = () => [
    facts([
      ["Start", timed(t.start, at0, t.start.value === null ? "" : utc(ctx.started, t.start.value))],
      ["First byte at", timed(t.firstByteAt, at0)],
      ["First token at", timed(t.firstTokenAt, at0)],
      ["End", timed(t.end, at0, t.end.value === null ? "" : utc(ctx.started, t.end.value))],
    ]),
    measured,
  ];
  const raw = () => [
    rawBytes("Request, rebuilt", ctx.base + "/exchange/" + n + "/request", c.sent_bytes[s]),
    rawBytes("Response, as kept", ctx.base + "/exchange/" + n + "/response", c.back_bytes[s]),
    facts([["Request as kept", bytes(c.kept_bytes[s])], ["API", c.api[s] ?? "-"], ["Upstream", c.upstream[s] ?? "-"]]),
  ];
  tabbed(
    node,
    [h("h3", {}, h("span", { class: "mark " + steps.tone(s) }), h("span", { class: "kind" }, "Step"), "#" + n, h("span", { class: "dim" }, c.thread[s] === null ? "In flight" : "Thread " + c.thread[s] + " · " + at + "/" + of))],
    [
      ["Summary", summary],
      ["Diff", () => diffTab(steps, s, ctx.base)],
      ["Options", () => config],
      ["Usage", usage],
      ["Timing", timing],
      ["Raw", raw],
    ],
    ctx,
  );
}

/** @param {HTMLElement} node @param {Entry} e @param {Context} ctx */
function inspectEntry(node, e, ctx) {
  const rawItem = () =>
    e.digest
      ? rawBytes("Item, as kept", ctx.base + "/item/" + e.digest, null)
      : rawBytes("The step's response, as kept", ctx.base + "/exchange/" + e.request + "/response", ctx.steps.cols.back_bytes[ctx.steps.row(e.request) ?? -1]);
  const head = [
    h("h3", {}, h("span", { class: "kind k-" + e.kind + (e.error ? " error" : ""), title: e.error ? "The tool reported a failure" : null }, e.kind), h("span", {}, (e.side === "added" ? "Added to" : "Answered by") + " request " + e.request), h("span", { class: "id dim" }, e.at)),
    h("p", { class: "links" }, h("button", { onclick: () => ctx.go(String(e.request)) }, "Go to its step")),
  ];
  // The step's reasoning, beside its message.
  const reasoning = () => {
    if (e.kind !== "assistant") return null;
    const row = ctx.steps.row(e.request);
    const of = row === undefined ? null : ctx.entriesOf(row);
    const thought = (of || []).filter((other) => other.side === "answer" && other.kind === "reasoning");
    if (!thought.length) return null;
    return h("details", { open: true }, h("summary", {}, "Reasoning"), thought.map((t) => h("pre", { class: "sent" }, marked(t.text, ctx.needle))));
  };
  if (e.kind === "call") {
    const output = e.output ? ctx.byAt.get(e.output) : undefined;
    const payload = () => sent(e.text, ctx.needle, ctx.api, e.at);
    const result = () =>
      output
        ? [h("p", { class: "links" }, h("button", { onclick: () => ctx.go(output.at) }, "Go to the output")), sent(output.text, ctx.needle, ctx.api, output.at)]
        : h("p", { class: "empty" }, e.output ? "Its output, " + e.output + ", isn't read yet: go to it to read it." : "No later step of this trace sends its output.");
    return tabbed(
      node,
      head,
      [
        ["Summary", () => [facts([["Name", e.name ?? "-"], ["Call ID", e.call_id ?? "-"], ["Output", e.output ?? "-"], ["Arguments", firstLine(e.text)]]), e.output ? h("p", { class: "links" }, h("button", { onclick: () => ctx.go(/** @type {string} */ (e.output)) }, "Go to the output")) : null]],
        ["Payload", payload],
        ["Result", result],
        ["Raw", rawItem],
      ],
      ctx,
    );
  }
  const goCall = e.call ? h("p", { class: "links" }, h("button", { onclick: () => ctx.go(/** @type {string} */ (e.call)) }, "Go to the call")) : null;
  return tabbed(
    node,
    head,
    [
      ["Text", () => [goCall, reasoning(), sent(e.text, ctx.needle, ctx.api, e.at)]],
      ["Raw", rawItem],
    ],
    ctx,
  );
}
