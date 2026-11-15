// A trace's Statistics tab: the server's `trace/<caller>/stats`, a row a step or a thread, with the
// caller's totals above, sortable by a column. The address keeps the grouping (`by=thread`), the
// order (`sort=<column>`, `-` first for the largest first) and the row selected (`srow=<request
// or thread>`); choosing a row selects its step or thread in the Steps tab (`go`).

import { count, fill, get, h, plural, route, setQuery, span, usd } from "./h.js";
import { RowList } from "./rows.js";

/**
 * @typedef {object} StatsReply `trace/<caller>/stats`: `rows` as columns of equal length
 * @property {string} caller
 * @property {"step" | "thread"} by
 * @property {Record<string, any[]>} rows
 * @property {Record<string, any>} totals the fold of every step
 * @typedef {object} Column
 * @property {string} name the server's column
 * @property {string} head
 * @property {string} title what it measures
 * @property {(value: any, rows: Record<string, any[]>, r: number) => string} text of row `r`'s value
 * @property {(rows: Record<string, any[]>, r: number) => string | null} [note] row `r`'s cell's hover
 * @property {string} [class]
 */

/** @param {number | null} ns */
const time = (ns) => (ns === null ? "-" : span(ns / 1e9));
/** @param {number | null} share */
const percent = (share) => (share === null ? "-" : Math.round(100 * share) + "%");
/** @param {number | null} rate */
const perSecond = (rate) => (rate === null ? "-" : Math.round(rate) + " tok/s");
/** @param {Record<string, number> | null} calls */
const callCount = (calls) => String(Object.values(calls || {}).reduce((sum, n) => sum + n, 0));
/** @param {number | null} n */
const number = (n) => (n === null ? "-" : n.toLocaleString("en-US"));

// A step's cost is unknown if its model has no price, or its response reported no tokens.
/** @param {number} unknown */
const unknownCost = (unknown) => plural(unknown, "step") + " of unknown cost";

/** @type {Column[]} */
const TIMES = [
  { name: "input", head: "In", title: "Input tokens", text: (v) => count(v, "tok"), class: "tok" },
  { name: "cache_hit", head: "Cached", title: "The share of the input tokens cached", text: percent, class: "pct" },
  { name: "output", head: "Out", title: "Output tokens", text: (v) => count(v, "tok"), class: "tok" },
];

/** @type {Record<"step" | "thread", Column>} */
const COST = {
  step: { name: "usd", head: "Cost", title: "What its tokens cost at its model's prices", text: usd, class: "usd" },
  thread: {
    name: "usd",
    head: "Cost",
    title: "The sum of its steps' costs",
    text: (v, rows, r) => usd(v) + (v !== null && rows.usd_unknown[r] ? "+" : ""),
    note: (rows, r) => (rows.usd_unknown[r] ? unknownCost(rows.usd_unknown[r]) : null),
    class: "usd",
  },
};

/** @type {Record<"step" | "thread", Column[]>} */
const COLUMNS = {
  step: [
    { name: "request", head: "#", title: "The step's request", text: (v) => "#" + v, class: "key" },
    { name: "thread", head: "Thread", title: "The root request of its thread", text: (v) => (v === null ? "-" : String(v)), class: "n" },
    ...TIMES,
    COST.step,
    { name: "to_first_byte_ns", head: "TTFB", title: "From its start to the response's first byte", text: time, class: "time" },
    { name: "to_first_token_ns", head: "TTFT", title: "From its start to the first token", text: time, class: "time" },
    { name: "decode_ns", head: "Decode", title: "From the first token to its end", text: time, class: "time" },
    { name: "model_ns", head: "Model", title: "From its start to its end", text: time, class: "time share" },
    { name: "queue_ns", head: "Before", title: "From its parent's end to its start: the time of its parent's tool calls and the harness", text: time, class: "time" },
    { name: "tok_s", head: "tok/s", title: "Output tokens a second of decoding, over a decode of 100 ms or more", text: perSecond, class: "rate" },
    { name: "calls", head: "Calls", title: "The tool calls of its answer", text: callCount, class: "n" },
    { name: "retry_of", head: "Retry of", title: "The request it retries, the same list sent again after a failure", text: (v) => (v === null ? "" : "#" + v), class: "word" },
    { name: "outcome", head: "Outcome", title: "How its request ended", text: (v) => String(v).replace("_", " "), class: "word" },
  ],
  thread: [
    { name: "thread", head: "Thread", title: "The root request of the thread", text: (v) => (v === null ? "In flight" : "#" + v), class: "key" },
    { name: "steps", head: "Steps", title: "Its steps", text: number, class: "n" },
    { name: "span_ns", head: "Span", title: "From its first step's start to its last step's end", text: time, class: "time" },
    ...TIMES,
    COST.thread,
    { name: "median_to_first_token_ns", head: "TTFT", title: "The median time to the first token", text: time, class: "time" },
    { name: "decode_ns", head: "Decode", title: "The sum of its steps' decoding", text: time, class: "time" },
    { name: "model_ns", head: "Model", title: "The sum of its steps' model times", text: time, class: "time share" },
    { name: "queue_ns", head: "Between", title: "The sum of the times between its steps: tool calls and the harness", text: time, class: "time" },
    { name: "tok_s", head: "tok/s", title: "Output tokens a second of decoding, over decodes of 100 ms or more", text: perSecond, class: "rate" },
    { name: "calls", head: "Calls", title: "The tool calls of its answers", text: callCount, class: "n" },
    { name: "retries", head: "Retries", title: "Its steps that retry a failed request", text: number, class: "n" },
    { name: "failures", head: "Failed", title: "Its steps whose upstream failed", text: number, class: "n" },
  ],
};

/** @param {Record<string, any>} t */
function totalsLine(t) {
  const part = (/** @type {string} */ name, /** @type {string} */ value, /** @type {string} */ title) => h("span", { class: "fact", title }, h("span", { class: "name" }, name), h("span", { class: "value" }, value));
  return [
    part("Steps", number(t.steps), "Its steps, in " + plural(t.threads, "thread") + " and " + plural(t.turns, "turn")),
    part("Threads", number(t.threads), "Its threads"),
    part("Tokens", count(t.input, "tok") + " in · " + percent(t.cache_hit) + " cached · " + (t.cache_write === null ? "" : count(t.cache_write, "tok") + " written to the cache · ") + count(t.output, "tok") + " out", "The sums of the tokens its steps report"),
    part("Cost", usd(t.usd), "The sum of its steps' costs" + (t.usd_unknown ? "; " + unknownCost(t.usd_unknown) : "")),
    part("Model", time(t.model_ns), "The sum of its steps' model times"),
    part("Between", time(t.queue_ns), "The sum of the times between steps: tool calls and the harness"),
    part("TTFT", time(t.median_to_first_token_ns), "The median time to the first token"),
    part("tok/s", perSecond(t.tok_s), "Output tokens a second of decoding"),
    part("Calls", callCount(t.calls), "Tool calls"),
    part("Retries", number(t.retries), "Steps that retry a failed request"),
    part("Repeats", number(t.repeats), "Steps that send again a list already answered"),
    part("Refused", number(t.refusals), "Steps that Pumpkin refused or barred"),
    part("Failed", number(t.failures), "Steps whose upstream failed"),
    t.in_flight ? part("In flight", number(t.in_flight), "Steps not answered yet") : null,
  ];
}

/** @param {{ api: string, go: (key: string) => void }} ctx */
export function statsView(ctx) {
  const totals = h("div", { class: "facts totals" });
  const head = h("div", { class: "srow shead" });
  const status = h("span", { class: "counted" });
  const byStep = h("button", { title: "A row a step", onclick: () => regroup("step") }, "By step");
  const byThread = h("button", { title: "A row a thread", onclick: () => regroup("thread") }, "By thread");
  const bar = h("div", { class: "filter toolbar" }, h("span", { class: "folds" }, byStep, byThread), status);
  /** @type {{ reply: StatsReply | null, order: number[], max: number }} */
  const state = { reply: null, order: [], max: 0 };
  const by = () => (route().query.get("by") === "thread" ? "thread" : "step");
  const columns = () => COLUMNS[by()];
  const keyOf = (/** @type {number} */ r) => String(/** @type {StatsReply} */ (state.reply).rows[columns()[0].name][r]);
  /** @type {RowList<number>} */
  const list = new RowList(
    (r) => {
      const rows = /** @type {StatsReply} */ (state.reply).rows;
      const cells = columns().map((column) => {
        const value = rows[column.name][r];
        const cell = h("span", { class: column.class || null, title: column.note?.(rows, r) ?? null }, column.text(value, rows, r));
        if (column.class?.includes("share") && value !== null && state.max) cell.style.setProperty("--share", String(value / state.max));
        return cell;
      });
      const failed = rows.outcome ? rows.outcome[r] !== "ok" : rows.failures[r] > 0;
      return h("div", { class: "srow" + (failed ? " failed" : "") }, cells);
    },
    (r) => choose(r, true),
  );
  const node = h("div", { class: "stats" }, totals, bar, h("div", { class: "table" }, head, list.node));

  const choose = (/** @type {number} */ r, /** @type {boolean} */ go) => {
    setQuery({ srow: keyOf(r) });
    list.select(state.order.indexOf(r), true);
    if (go) ctx.go(by() === "thread" ? "t" + keyOf(r) : keyOf(r));
  };
  const regroup = (/** @type {"step" | "thread"} */ to) => {
    setQuery({ by: to === "step" ? null : to, sort: null, srow: null });
    read();
  };
  const sortBy = (/** @type {string} */ name) => {
    const sort = route().query.get("sort");
    // The largest first, then the smallest first, then the server's order.
    setQuery({ sort: sort === "-" + name ? name : sort === name ? null : "-" + name });
    draw();
  };

  const draw = () => {
    const reply = state.reply;
    if (!reply) return;
    const { query } = route();
    fill(totals, totalsLine(reply.totals));
    byStep.classList.toggle("on", by() === "step");
    byThread.classList.toggle("on", by() === "thread");
    const sort = query.get("sort") || "";
    const name = sort.replace(/^-/, "");
    fill(
      head,
      columns().map((column) =>
        h("button", { class: (column.class || "") + (name === column.name ? " on" : ""), title: column.title + " (sorts by it)", onclick: () => sortBy(column.name) }, column.head + (name === column.name ? (sort.startsWith("-") ? " ↓" : " ↑") : "")),
      ),
    );
    const rows = reply.rows;
    const length = rows[columns()[0].name].length;
    state.order = [...Array(length).keys()];
    const values = rows[name];
    if (values) {
      const numeric = (/** @type {any} */ v) => (v === null ? null : typeof v === "object" ? Object.values(v).reduce((sum, n) => sum + n, 0) : v);
      const down = sort.startsWith("-") ? -1 : 1;
      // Unknown values last, either way.
      state.order.sort((a, b) => {
        const [x, y] = [numeric(values[a]), numeric(values[b])];
        if (x === null || y === null) return (x === null ? 1 : 0) - (y === null ? 1 : 0);
        return x < y ? -down : x > y ? down : a - b;
      });
    }
    state.max = Math.max(0, ...rows.model_ns.map((v) => v ?? 0));
    status.textContent = plural(length, by());
    const selected = query.get("srow");
    list.set(state.order, selected === null ? -1 : state.order.findIndex((r) => keyOf(r) === selected));
    if (selected !== null) list.select(list.selected, true);
  };

  const read = async () => {
    status.textContent = "Reading the statistics…";
    try {
      state.reply = await get(ctx.api + "/stats?by=" + by());
    } catch (error) {
      status.textContent = "Reading the statistics failed: " + (error instanceof Error ? error.message : String(error));
      return;
    }
    draw();
  };

  return {
    node,
    // Reads the statistics again, as a live run's grow.
    show: read,
    // j and k select the next and previous row, Enter shows it in the Steps tab.
    key: (/** @type {KeyboardEvent} */ event) => {
      const by = { j: 1, k: -1 }[event.key];
      const at = list.selected + (by ?? 0);
      if (by && at >= 0 && at < state.order.length) choose(state.order[at], false);
      else if (event.key === "Enter" && list.selected >= 0) choose(state.order[list.selected], true);
    },
  };
}
