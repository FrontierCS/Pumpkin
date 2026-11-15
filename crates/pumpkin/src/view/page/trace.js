// A caller's trace: its model exchanges as threads, steps and entries, in a timeline
// ([`timeline.js`]) above a ledger at left and an inspector at right, with what is shown and
// selected in the address (`#/run/<id>/trace?caller=agent&fold=steps&sel=12/out/1&q=…`), so
// that a link shows the same.
// Its Statistics and Log tabs are [`stats.js`]'s and [`log.js`]'s, `view=stats` and `view=log` in
// the address.
//
// The steps are read whole, as columns, in one reply; the entries of the steps in view are read
// in windows once the ledger's scroll pauses on them, a window that scrolls away before it comes
// being dropped, and the windows read last are kept; a step's header is drawn at once, its
// entries when they come. While the run is live, the steps are polled from where the last read
// ended: a step that comes again replaces its row, and its entries are read again, the old ones
// shown until the new come.

import { POLL_MS, Refused, byId, fill, follow, get, h, keepSetting, plural, route, sentence, setKeys, setQuery, setting, show, tone, typing } from "./h.js";
import { forgetExchanges, inspect } from "./inspector.js";
import { drawRow, keyOf, layout, matches, stepFolded, threadFolded } from "./ledger.js";
import { logView } from "./log.js";
import { RowList } from "./rows.js";
import { statsView } from "./stats.js";
import { IN_FLIGHT, Steps } from "./steps.js";
import { Timeline } from "./timeline.js";

/**
 * @typedef {import("./steps.js").Entry} Entry
 * @typedef {import("./steps.js").StepsReply} StepsReply
 * @typedef {import("./ledger.js").Row} Row
 * @typedef {import("./ledger.js").Shape} Shape
 * @typedef {import("./ledger.js").Marks} Marks
 * @typedef {import("./inspector.js").Selected} Selected
 * @typedef {import("./timeline.js").OpColumns} OpColumns
 * @typedef {{ at: string, request: number, index: number, kind: string, byte: number, context: [string, string, string] }} Hit
 * @typedef {{ controller: AbortController, requests: Set<number> }} Reading
 */

// The kinds of entry the server names.
const KINDS = ["system", "user", "assistant", "reasoning", "call", "output", "other", "unread"];
// The steps of a thread read in one window of entries, at most.
const WINDOW_STEPS = 32;
// The windows read at once: the server answers one request at a time.
const READING_AT_ONCE = 2;
// The rows past those in view whose window, still being read, is kept going.
const KEEP_ROWS = 80;
// How long a step's row stays in view before its window is read, in ms: a slow scroll keeps a row
// in view for longer, a fling passes over rows it never shows.
const SETTLE_MS = 100;
// The windows of entries kept, those read last; one further from view is dropped, and read again
// when shown.
const KEPT_WINDOWS = 48;
// The inspector's width, its bounds and its default, in px, which is less in a narrow window.
const INSPECTOR_PX = { min: 320, max: 720, default: 440, share: 0.34 };
const inspectorDefault = () => Math.min(INSPECTOR_PX.default, Math.round(innerWidth * INSPECTOR_PX.share));

/** @typedef {"trace" | "stats" | "log"} View */
/** @returns {View} */
function viewOf() {
  const view = route().query.get("view");
  return view === "stats" || view === "log" ? view : "trace";
}

// The trace's shape as the address says it.
/** @returns {Shape} */
function shapeOf() {
  const { query } = route();
  const set = (/** @type {string} */ name) => new Set((query.get(name) || "").split(",").filter(Boolean).map(Number));
  const fold = query.get("fold");
  return {
    fold: fold === "calls" || fold === "steps" || fold === "threads" ? fold : "",
    threads: set("ot"),
    steps: set("os"),
    thread: /^\d+$/.test(query.get("thread") || "") ? Number(query.get("thread")) : null,
    kind: KINDS.includes(query.get("kind") || "") ? /** @type {string} */ (query.get("kind")) : "",
  };
}

/** @param {string} id */
export async function traceView(id) {
  const base = "/api/runs/" + encodeURIComponent(id);
  const caller = route().query.get("caller") || "agent";
  const api = base + "/trace/" + encodeURIComponent(caller);
  const state = {
    /** @type {any} */
    run: null,
    /** @type {Steps | null} */
    steps: null,
    /** @type {OpColumns | null} the ops the run brokered for the caller */
    ops: null,
    end: 0,
    epoch: 0,
    /** @type {Map<string, Map<number, Entry[]>>} the entries read, by the kind read ("" every kind), then request */
    read: new Map(),
    /** @type {Map<string, Entry>} */
    byAt: new Map(),
    /** @type {Reading[]} */
    reading: [],
    /** @type {Set<number>} the steps whose entries are to be read again, shown as read until then */
    stale: new Set(),
    /** @type {Array<{ kind: string, requests: number[] }>} the windows read, the last read last */
    windows: [],
    /** @type {Row[]} */
    rows: [],
    /** @type {Hit[]} */
    hits: [],
    hitCount: 0,
    hitAt: -1,
    /** @type {Marks | null} */
    marks: null,
    searched: "",
    // The selection the address names whose entries aren't read yet.
    /** @type {string | null} */
    waiting: null,
    /** @type {Map<string, string>} the inspector's tab last chosen for each kind of selection */
    tabs: new Map(),
  };
  let shape = shapeOf();
  const { query } = route();

  // The run's clock now, in seconds, while it is live, which a step in flight reaches to.
  const now = () => {
    const started = state.run?.times?.started_unix;
    return state.run?.live && typeof started === "number" ? Date.now() / 1000 - started : null;
  };
  const timeline = new Timeline(
    () => ({ steps: state.steps, ops: state.ops, thread: shape.thread, hits: state.marks?.steps ?? null, sel: route().query.get("sel"), now: now() }),
    (request) => select(String(request), true),
    () => focusRange(),
  );

  const title = h("h2", { class: "title" }, caller.replace("-", " "));
  const within = h("p", { class: "within" });
  const callers = h("span", { class: "callers" });
  const tabButton = (/** @type {View} */ view, /** @type {string} */ name) => h("button", { role: "tab", onclick: () => showView(view) }, name);
  /** @type {Record<View, HTMLElement>} */
  const tabButtons = { trace: tabButton("trace", "Steps"), stats: tabButton("stats", "Statistics"), log: tabButton("log", "Log") };
  const tabs = h("div", { class: "tabs", role: "tablist" }, Object.values(tabButtons), callers);
  const search = /** @type {HTMLInputElement} */ (h("input", { type: "search", class: "search", placeholder: "Search the trace  ( / )", title: "Search every entry's text (/ to type, Enter and Shift-Enter for the next and previous match, Esc to leave)", value: query.get("q") || "" }));
  const found = h("span", { class: "counted" });
  const prev = h("button", { title: "The previous match (Shift-Enter)", onclick: () => goHit(-1) }, "↑");
  const next = h("button", { title: "The next match (Enter)", onclick: () => goHit(1) }, "↓");
  const folds = h("span", { class: "folds" });
  const kinds = /** @type {HTMLSelectElement} */ (h("select", { title: "Show the entries of one kind" }, h("option", { value: "" }, "Every kind"), KINDS.map((kind) => h("option", { value: kind }, kind))));
  kinds.value = shape.kind;
  const threads = /** @type {HTMLSelectElement} */ (h("select", { title: "Show one thread" }));
  const status = h("span", { class: "counted status" }, "Reading the steps of " + caller.replace("-", " ") + "…");
  const toolbar = h("div", { class: "filter toolbar" }, search, prev, next, found, folds, kinds, threads, status);
  const inspector = h("div", { class: "inspector panel" });
  const grip = h("div", { class: "grip", tabindex: 0, role: "separator", "aria-orientation": "vertical", title: "Drag to widen the inspector (arrows too; double-click resets)" });
  const startedOver = h("p", { class: "empty" });
  /** @type {RowList<Row>} */
  const list = new RowList(
    (row) => {
      const steps = /** @type {Steps} */ (state.steps);
      const node = drawRow(row, { steps, shape, byAt: state.byAt, marks: state.marks, started: state.run?.times?.started_unix ?? null, toggle });
      if (row.k === "thread" || row.k === "step") node.addEventListener("dblclick", () => toggle(row));
      // Outside the timeline's range, a step's rows dim.
      const range = timeline.range;
      const request = row.k === "thread" ? null : steps.cols.request[row.s];
      if (range && request !== null && (request < range.from || request > range.to)) node.classList.add("out");
      return node;
    },
    (row) => {
      if (row.k === "calls") return setFold("");
      const key = keyOf(/** @type {Steps} */ (state.steps), row);
      if (key) select(key, false);
    },
    (first, end) => seen(first, end),
  );
  const split = h("div", { class: "trace-split" + (caller === "agent" ? "" : " of-judge") }, h("div", { class: "left" }, list.node), grip, inspector);
  const width = Number(setting("inspector")) || inspectorDefault();
  const widen = (/** @type {number} */ px) => {
    const kept = Math.round(Math.min(INSPECTOR_PX.max, Math.max(INSPECTOR_PX.min, px)));
    split.style.setProperty("--inspector", kept + "px");
    return kept;
  };
  widen(width);
  const pane = h("div", { class: "pane" }, toolbar, startedOver, timeline.node, split);
  // The Statistics and Log tabs, each made when first shown.
  /** @type {Partial<Record<View, { node: HTMLElement, show: () => void, key: (event: KeyboardEvent) => void }>>} */
  const others = {};
  const panes = h("div", { class: "pane" });
  show(title, within, tabs, pane, panes);
  const showView = (/** @type {View} */ view) => {
    setQuery({ view: view === "trace" ? null : view });
    for (const [name, button] of Object.entries(tabButtons)) {
      button.classList.toggle("on", name === view);
      button.setAttribute("aria-selected", String(name === view));
    }
    pane.hidden = view !== "trace";
    panes.hidden = view === "trace";
    if (view === "trace") return list.paint(true);
    const go = (/** @type {string} */ key) => (showView("trace"), select(key, true));
    const started = () => state.run?.times?.started_unix ?? null;
    const other = (others[view] ??= view === "stats" ? statsView({ api, go }) : logView({ base, caller, started, go }));
    fill(panes, other.node);
    other.show();
  };

  // ---- the inspector's width: dragged, stepped by arrows, reset by a double-click

  grip.addEventListener("pointerdown", (event) => {
    grip.setPointerCapture(event.pointerId);
    const move = (/** @type {PointerEvent} */ moved) => widen(split.getBoundingClientRect().right - moved.clientX);
    const up = () => {
      grip.removeEventListener("pointermove", move);
      keepSetting("inspector", String(widen(inspector.getBoundingClientRect().width)));
    };
    grip.addEventListener("pointermove", move);
    grip.addEventListener("pointerup", up, { once: true });
  });
  grip.addEventListener("dblclick", () => keepSetting("inspector", String(widen(inspectorDefault()))));
  grip.addEventListener("keydown", (event) => {
    const by = { ArrowLeft: 16, ArrowRight: -16 }[event.key];
    if (by) keepSetting("inspector", String(widen(inspector.getBoundingClientRect().width + by)));
  });

  // ---- the entries: read in windows of the steps in view, and kept

  // The entries read of the step at row `s`, of the kind shown, or null if not read yet.
  const entriesOf = (/** @type {number} */ s, kind = shape.kind) => {
    const request = /** @type {Steps} */ (state.steps).cols.request[s];
    const every = state.read.get("")?.get(request);
    if (every) return kind ? every.filter((e) => e.kind === kind) : every;
    return kind ? state.read.get(kind)?.get(request) ?? null : null;
  };
  const isReading = (/** @type {number} */ request) => state.reading.some((r) => r.requests.has(request));
  // Whether the step at row `s` is to be read: never read of the kind shown, or read before it
  // came again.
  const unread = (/** @type {number} */ s) => entriesOf(s) === null || state.stale.has(/** @type {Steps} */ (state.steps).cols.request[s]);

  /** @type {number | undefined} */
  let settling;
  /** @type {Set<number>} the steps near the view as last seen */
  let near = new Set();
  /** @type {Map<number, number>} when each step in view to be read came into view, by its row */
  let since = new Map();
  // Notes the rows from `first` to `end` in view: drops the windows being read that scrolled far
  // away, and reads those that stay in view.
  const seen = (/** @type {number} */ first, /** @type {number} */ end) => {
    const steps = state.steps;
    if (!steps) return;
    near = new Set();
    for (let at = Math.max(0, first - KEEP_ROWS); at < Math.min(state.rows.length, end + KEEP_ROWS); at++) {
      const row = state.rows[at];
      if (row.k !== "thread") near.add(steps.cols.request[row.s]);
    }
    for (const reading of state.reading) {
      if (![...reading.requests].some((request) => near.has(request))) reading.controller.abort();
    }
    const now = performance.now();
    const was = since;
    since = new Map();
    for (let at = first; at < end; at++) {
      const row = state.rows[at];
      // A step's own row or one of its entries' rows, the step's header above the view.
      if (row && row.k !== "thread" && !stepFolded(shape, steps.cols.request[row.s]) && unread(row.s)) since.set(row.s, was.get(row.s) ?? now);
    }
    readInView();
  };
  // Reads the windows of the steps that have been in view for `SETTLE_MS`, and waits for those
  // that haven't yet.
  const readInView = () => {
    if (!state.steps || !list.node.isConnected) return;
    const c = state.steps.cols;
    const now = performance.now();
    let wait = Infinity;
    for (const [s, from] of since) {
      if (!unread(s) || isReading(c.request[s])) continue;
      if (now - from < SETTLE_MS) wait = Math.min(wait, SETTLE_MS - (now - from));
      else if (state.reading.length < READING_AT_ONCE) readWindow(s);
    }
    clearTimeout(settling);
    if (wait < Infinity) settling = setTimeout(readInView, wait);
  };
  // Keeps `requests`' window, of `kind`, as read last, and drops the entries of the windows read
  // first past `KEPT_WINDOWS`, but those near the view.
  const keep = (/** @type {string} */ kind, /** @type {number[]} */ requests) => {
    const held = new Set(requests);
    state.windows = state.windows.filter((w) => w.kind !== kind || !w.requests.some((r) => held.has(r)));
    state.windows.push({ kind, requests });
    for (let at = 0; state.windows.length > KEPT_WINDOWS && at < state.windows.length; ) {
      const w = state.windows[at];
      if (w.requests.some((r) => near.has(r))) {
        at++;
        continue;
      }
      state.windows.splice(at, 1);
      const read = state.read.get(w.kind);
      for (const request of w.requests) {
        for (const e of read?.get(request) || []) if (state.byAt.get(e.at) === e) state.byAt.delete(e.at);
        read?.delete(request);
      }
    }
  };

  // Reads the window of the steps of `s`'s thread around it whose entries aren't read.
  const readWindow = async (/** @type {number} */ s) => {
    const steps = /** @type {Steps} */ (state.steps);
    const c = steps.cols;
    const root = c.thread[s] ?? IN_FLIGHT;
    const of = /** @type {number[]} */ (steps.threads.get(root));
    const at = of.indexOf(s);
    const block = root === IN_FLIGHT ? [s] : of.slice(at - (at % WINDOW_STEPS), at - (at % WINDOW_STEPS) + WINDOW_STEPS);
    const rows = block.filter((row) => unread(row) && !isReading(c.request[row]));
    if (!rows.length) return;
    const requests = new Set(rows.map((row) => c.request[row]));
    const kind = shape.kind;
    const reading = { controller: new AbortController(), requests };
    state.reading.push(reading);
    const from = c.request[rows[0]];
    const to = c.request[rows[rows.length - 1]];
    const one = (steps.reply.threads > 1 && root !== IN_FLIGHT ? "&thread=" + root : "") + (kind ? "&kind=" + kind : "");
    /** @type {Map<number, Entry[]>} */
    const got = new Map([...requests].map((request) => [request, []]));
    try {
      /** @type {{ request: number, entry: number } | null} */
      let resume = null;
      do {
        const more = resume ? "&from=" + resume.request + "&entry=" + resume.entry : "&from=" + from;
        /** @type {{ entries: Entry[], next: { request: number, entry: number } | null }} */
        const window = await get(api + "/entries?to=" + to + more + one, reading.controller.signal);
        for (const e of window.entries) got.get(e.request)?.push(e);
        resume = window.next;
      } while (resume);
    } catch (error) {
      if (!(error instanceof DOMException && error.name === "AbortError")) status.textContent = "Reading entries failed: " + (error instanceof Error ? error.message : String(error));
      return;
    } finally {
      state.reading.splice(state.reading.indexOf(reading), 1);
    }
    if (state.steps !== steps) return;
    let into = state.read.get(kind);
    if (!into) state.read.set(kind, (into = new Map()));
    for (const [request, entries] of got) {
      into.set(request, entries);
      state.stale.delete(request);
      for (const e of entries) state.byAt.set(e.at, e);
    }
    keep(kind, [...got.keys()]);
    redraw();
    readInView();
    if (state.waiting) {
      const waiting = state.waiting;
      if (state.byAt.has(waiting)) select(waiting, true);
    }
  };

  // ---- drawing

  // Lays the rows out again, keeping the step at the top of the view where it was.
  const redraw = () => {
    const steps = state.steps;
    if (!steps) return;
    const old = state.rows;
    const top = list.top();
    const anchor = old[top];
    const anchorKey = anchor ? (anchor.k === "thread" ? "t" + anchor.root : "s" + anchor.s) : null;
    let into = 0;
    for (let at = top; anchor && anchor.k !== "thread" && at > 0 && old[at].k !== "step"; at--) into++;
    state.rows = layout(steps, (s) => entriesOf(s), shape);
    /** @type {{ from: number, to: number } | null} */
    let moved = null;
    if (anchorKey && !list.tail) {
      const at = state.rows.findIndex((row) => (row.k === "thread" ? "t" + row.root : row.k === "step" ? "s" + row.s : null) === anchorKey);
      if (at >= 0) moved = { from: top, to: at + into };
    }
    const sel = route().query.get("sel");
    list.set(state.rows, sel ? state.rows.findIndex((row) => keyOf(steps, row) === sel) : -1, moved);
    drawBar();
    timeline.draw();
  };

  // Dims the rows outside the timeline's range, and scrolls its first step into view.
  const focusRange = () => {
    const steps = state.steps;
    const range = timeline.range;
    list.paint(true);
    if (!steps || !range) return;
    const at = state.rows.findIndex((row) => row.k === "step" && steps.cols.request[row.s] >= range.from && steps.cols.request[row.s] <= range.to);
    if (at < 0) return;
    list.tail = false;
    list.node.scrollTop = at * list.px - list.node.clientHeight / 3;
  };

  const drawBar = () => {
    const steps = state.steps;
    if (!steps) return;
    const level = (/** @type {"threads" | "steps" | "calls"} */ fold, /** @type {string} */ title) =>
      h("button", { class: shape.fold === fold ? "on" : null, title, onclick: () => setFold(shape.fold === fold ? "" : fold) }, sentence(fold));
    fill(folds, h("span", { class: "dim" }, "Fold"), steps.reply.threads > 1 ? level("threads", "Fold every thread to its header") : null, level("steps", "Fold every step to its header"), level("calls", "Fold each step's tool calls and outputs into one row"));
    const reading = state.reading.length ? " · reading entries…" : "";
    status.textContent = plural(steps.length, "step") + (steps.reply.threads > 1 ? " · " + plural(steps.reply.threads, "thread") : "") + reading;
  };

  const drawThreads = () => {
    const steps = /** @type {Steps} */ (state.steps);
    threads.hidden = steps.threads.size < 2;
    if (threads.hidden) return;
    const options = [...steps.threads].map(([root, of]) => h("option", { value: String(root) }, (root === IN_FLIGHT ? "In flight" : "Thread " + root) + " · " + plural(of.length, "step")));
    fill(threads, h("option", { value: "" }, "Every thread"), options);
    threads.value = shape.thread === null ? "" : String(shape.thread);
  };

  const drawRun = () => {
    const run = state.run;
    // A live run is only called `live`: its verdict, read once, would go stale. An ended run's
    // verdict is read again, past the run's id that starts it and the path already names.
    const named = "Run " + run.id + " ";
    const how = run.live ? "live" : run.verdict.startsWith(named) ? run.verdict.slice(named.length) : run.verdict;
    fill(within, "In run ", h("a", { href: "#/run/" + encodeURIComponent(id), title: "The run" }, h("span", { class: "mark " + tone(run) }), how));
    const lanes = /** @type {any[]} */ (run.lanes || []).filter((lane) => (lane.kind === "agent" || lane.kind === "judge") && lane.answers && lane.answers.count > 0);
    fill(
      callers,
      lanes.length > 1 ? h("span", { class: "dim" }, "Other traces") : null,
      lanes.length > 1 ? lanes.map((lane) => h("a", { href: "#/run/" + encodeURIComponent(id) + "/trace?caller=" + encodeURIComponent(lane.caller), class: "kind " + lane.kind + (lane.caller === caller ? " on" : ""), title: "The trace of " + lane.lane }, lane.lane)) : null,
    );
    drawFollow();
  };
  const drawFollow = () => {
    const live = !!state.run && state.run.live;
    fill(
      byId("follow"),
      live ? h("span", { class: "mark live", title: "The run is live: its steps are read every " + POLL_MS / 1000 + " s" }) : null,
      live ? (list.tail ? h("button", { title: "Stop keeping to the end (f)", onclick: () => ((list.tail = false), drawFollow()) }, "Following the end") : h("button", { title: "Go to the end of the trace, and keep to it (f)", onclick: () => (list.end(), drawFollow()) }, "Follow the end")) : null,
    );
  };
  list.node.addEventListener("scroll", () => drawFollow());

  // ---- selection

  /** @returns {Selected} */
  const selectedOf = (/** @type {string | null} */ key) => {
    const steps = /** @type {Steps} */ (state.steps);
    if (!key) return { k: "none" };
    if (key.startsWith("t")) return { k: "thread", root: Number(key.slice(1)) };
    if (/^\d+$/.test(key)) {
      const s = steps.row(Number(key));
      return s === undefined ? { k: "none" } : { k: "step", s };
    }
    const e = state.byAt.get(key);
    return e ? { k: "entry", e } : { k: "none" };
  };
  const drawInspector = () => {
    const steps = state.steps;
    if (!steps) return;
    const { query } = route();
    const selected = selectedOf(query.get("sel"));
    // A step, a call and another entry each keep the tab last chosen for one of their kind.
    const kind = selected.k === "entry" ? (selected.e.kind === "call" ? "call" : "entry") : selected.k;
    inspect(inspector, selected, {
      steps,
      base,
      caller,
      api,
      byAt: state.byAt,
      entriesOf: (s) => entriesOf(s, ""),
      started: state.run?.times?.started_unix ?? null,
      needle: state.searched,
      tab: state.tabs.get(kind) ?? query.get("tab"),
      setTab: (tab) => (state.tabs.set(kind, tab), setQuery({ tab }), drawInspector()),
      go: (key) => select(key, true),
    });
  };

  // Selects what `key` addresses, opening the folds that hide it and reading its entries.
  const select = (/** @type {string | null} */ key, /** @type {boolean} */ scroll) => {
    const steps = state.steps;
    if (!steps) return;
    setQuery({ sel: key });
    state.waiting = null;
    if (key && !key.startsWith("t")) {
      const request = Number(key.split("/")[0]);
      const s = steps.row(request);
      if (s !== undefined) {
        const root = steps.cols.thread[s] ?? IN_FLIGHT;
        let opened = false;
        if (shape.thread !== null && shape.thread !== root) (shape.thread = null), setQuery({ thread: null }), (opened = true);
        if (steps.reply.threads > 1 && threadFolded(shape, root)) toggleIn(shape.threads, root, "ot"), (opened = true);
        if (key.includes("/") && stepFolded(shape, request)) toggleIn(shape.steps, request, "os"), (opened = true);
        if (key.includes("/") && !state.byAt.has(key)) {
          state.waiting = key;
          if (!isReading(request)) readWindow(s);
        }
        if (opened) redraw();
      }
    }
    const index = key ? state.rows.findIndex((row) => keyOf(steps, row) === key) : -1;
    if (index < 0 && state.waiting) {
      const header = state.rows.findIndex((row) => row.k === "step" && steps.cols.request[row.s] === Number(key?.split("/")[0]));
      list.select(-1, false);
      if (header >= 0) list.node.scrollTop = header * list.px - list.node.clientHeight / 3;
    } else list.select(index, scroll);
    if (key) list.tail = false;
    if (key && !key.startsWith("t")) timeline.reveal(Number(key.split("/")[0]));
    timeline.draw();
    drawInspector();
  };

  // ---- folds and filters

  /** @param {Set<number>} set @param {number} value @param {string} name */
  const toggleIn = (set, value, name) => {
    if (!set.delete(value)) set.add(value);
    setQuery({ [name]: [...set].join(",") || null });
  };
  const toggle = (/** @type {Row} */ row) => {
    const steps = /** @type {Steps} */ (state.steps);
    if (row.k === "thread") toggleIn(shape.threads, row.root, "ot");
    else if (row.k === "step") toggleIn(shape.steps, steps.cols.request[row.s], "os");
    else if (row.k === "calls") return setFold("");
    redraw();
  };
  const setFold = (/** @type {"" | "calls" | "steps" | "threads"} */ fold) => {
    setQuery({ fold: fold || null, ot: null, os: null });
    shape = shapeOf();
    redraw();
  };
  kinds.addEventListener("change", () => {
    setQuery({ kind: kinds.value || null });
    shape = shapeOf();
    redraw();
    if (state.searched) runSearch(state.searched);
  });
  threads.addEventListener("change", () => {
    setQuery({ thread: threads.value || null });
    shape = shapeOf();
    timeline.reset();
    timeline.take();
    list.node.scrollTop = 0;
    redraw();
    if (state.searched) runSearch(state.searched);
  });

  // ---- search: in the server, over every entry's text

  /** @type {number | undefined} */
  let typed;
  const runSearch = async (/** @type {string} */ text) => {
    if (!list.node.isConnected) return;
    const needle = text.trim() ? text : "";
    setQuery({ q: needle || null });
    state.searched = needle;
    if (!needle) {
      Object.assign(state, { hits: [], hitCount: 0, hitAt: -1, marks: null });
      found.textContent = "";
      list.paint(true);
      timeline.draw();
      return;
    }
    found.textContent = "Searching…";
    const one = (shape.kind ? "&kind=" + shape.kind : "") + (shape.thread !== null ? "&thread=" + shape.thread : "");
    /** @type {{ count: number, hits: Hit[] }} */
    let reply;
    try {
      reply = await get(api + "/search?q=" + encodeURIComponent(needle) + one);
    } catch (error) {
      if (state.searched !== needle) return;
      Object.assign(state, { hits: [], hitCount: 0, hitAt: -1, marks: null });
      found.textContent = (error instanceof Refused ? "Search refused: " : "Search failed: ") + (error instanceof Error ? error.message : String(error));
      list.paint(true);
      return;
    }
    if (state.searched !== needle) return;
    /** @type {Map<number, number>} */
    const steps = new Map();
    for (const hit of reply.hits) steps.set(hit.request, (steps.get(hit.request) || 0) + 1);
    Object.assign(state, { hits: reply.hits, hitCount: reply.count, hitAt: -1, marks: { at: new Set(reply.hits.map((hit) => hit.at)), steps } });
    drawFound();
    list.paint(true);
    timeline.draw();
    drawInspector();
  };
  const drawFound = () => {
    const { hits, hitCount, hitAt } = state;
    if (!hitCount) return void (found.textContent = state.searched ? "No match" : "");
    found.textContent = (hitAt >= 0 ? hitAt + 1 + " of " : "") + matches(hitCount) + (hits.length < hitCount ? " (the first " + hits.length.toLocaleString("en-US") + " marked)" : "");
  };
  const goHit = async (/** @type {number} */ by) => {
    if ((search.value.trim() ? search.value : "") !== state.searched) await runSearch(search.value);
    if (!state.hits.length) return;
    const n = state.hits.length;
    state.hitAt = state.hitAt < 0 ? (by > 0 ? 0 : n - 1) : (state.hitAt + by + n) % n;
    drawFound();
    select(state.hits[state.hitAt].at, true);
  };
  search.addEventListener("input", () => {
    clearTimeout(typed);
    typed = setTimeout(() => list.node.isConnected && runSearch(search.value), 250);
  });
  search.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      clearTimeout(typed);
      goHit(event.shiftKey ? -1 : 1);
    } else if (event.key === "Escape") {
      event.preventDefault();
      search.blur();
    }
  });

  // ---- keys

  // The next row from the selected one, `by` rows on, that `wanted` keeps.
  const step = (/** @type {number} */ by, /** @type {(row: Row) => boolean} */ wanted) => {
    const steps = /** @type {Steps} */ (state.steps);
    for (let at = list.selected + by; at >= 0 && at < state.rows.length; at += by) {
      const row = state.rows[at];
      if (wanted(row) && keyOf(steps, row)) return select(keyOf(steps, row), true);
    }
  };
  setKeys((event) => {
    const view = viewOf();
    if (view !== "trace") return void (typing() || others[view]?.key(event));
    if (typing() || !state.steps) return;
    if (event.key === "/") {
      event.preventDefault();
      search.focus();
      search.select();
    } else if (event.key === "Enter") {
      // Enter on a button or a link presses it.
      if (event.target instanceof Element && event.target.closest("button, a")) return;
      event.preventDefault();
      goHit(event.shiftKey ? -1 : 1);
    } else if (event.key === "j" || event.key === "k") {
      event.preventDefault();
      step(event.key === "j" ? 1 : -1, () => true);
    } else if (event.key === "]" || event.key === "[") {
      event.preventDefault();
      step(event.key === "]" ? 1 : -1, (row) => row.k === "step");
    } else if (event.key === "f" && state.run && state.run.live) {
      if (list.tail) list.tail = false;
      else list.end();
      drawFollow();
    } else if (event.key === "Escape") {
      if (timeline.clearRange()) return;
      if (route().query.get("sel")) select(null, false);
      else if (state.searched) (search.value = ""), runSearch("");
    }
  });

  // ---- reading the steps, and following them while the run is live

  // The log started over: the steps are read again, and what the address names by thread, step or
  // entry is dropped, as the new log's requests needn't be the old ones.
  const startOver = () => {
    for (const reading of state.reading) reading.controller.abort();
    Object.assign(state, { steps: null, ops: null, end: 0, epoch: 0, read: new Map(), byAt: new Map(), reading: [], stale: new Set(), windows: [], rows: [], waiting: null });
    forgetExchanges();
    setQuery({ thread: null, ot: null, os: null, sel: null });
    shape = shapeOf();
    startedOver.textContent = "The run's log started over: the trace is read again from its start.";
  };
  // The run ended: its verdict is read again.
  const ended = async () => {
    state.run = await get(base);
    drawRun();
  };
  const read = async (/** @type {() => boolean} */ current) => {
    if (!state.steps) {
      const [run, reply, ops] = await Promise.all([state.run ? Promise.resolve(state.run) : get(base), /** @type {Promise<StepsReply>} */ (get(api + "/steps")), get(api + "/ops")]);
      if (!current()) return null;
      state.run = run;
      state.run.live = reply.live;
      state.steps = new Steps(reply);
      state.ops = ops.ops;
      timeline.take();
      Object.assign(state, { end: reply.end, epoch: reply.log_epoch });
      drawRun();
      drawThreads();
      const sel = route().query.get("sel");
      if (run.live && !sel) list.tail = true;
      redraw();
      if (sel) select(sel, true);
      else drawInspector();
      if (timeline.range && !sel) focusRange();
      if (state.searched || search.value) runSearch(search.value);
      return reply.live ? POLL_MS : null;
    }
    /** @type {[StepsReply, { ops: OpColumns }]} */
    const [reply, ops] = await Promise.all([get(api + "/steps?since=" + state.end + "&log_epoch=" + state.epoch), get(api + "/ops")]);
    if (!current()) return null;
    state.end = reply.end;
    state.ops = ops.ops;
    const steps = state.steps;
    // The entries of the steps in flight are read again at each poll: their answers grow.
    const flying = steps.cols.request.filter((_, s) => steps.inFlight(s));
    // Their entries stay shown until read again.
    const forget = (/** @type {number[]} */ requests) => {
      for (const request of requests) if ([...state.read.values()].some((read) => read.has(request))) state.stale.add(request);
    };
    if (!reply.steps.request.length) {
      // Steps in flight and ops reach to now.
      timeline.take();
      if (flying.length) forget(flying), redraw();
      if (!reply.live) await ended();
      return reply.live ? POLL_MS : null;
    }
    // A row that comes again has its entries read again.
    forget([...steps.merge(reply), ...flying]);
    timeline.take();
    drawThreads();
    redraw();
    readInView();
    if (!route().query.get("sel")) drawInspector();
    if (!reply.live) return await ended(), null;
    return POLL_MS;
  };
  showView(viewOf());
  follow(async (current) => {
    try {
      return await read(current);
    } catch (error) {
      if (error instanceof Refused && error.status === 409 && state.steps) {
        startOver();
        return 0;
      }
      throw error;
    }
  });
}
