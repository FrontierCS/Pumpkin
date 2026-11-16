// A trace's timeline: its steps across, in three lanes, Model (each step's span, in its first
// byte, its first token and its decoding), Tools (the time between a step that called tools and
// the step that sends their outputs) and Ops (what the run brokered for the caller), over an
// axis and a strip of the whole trace that holds the window in view. By step, every step is as
// wide as the next, so that a long trace is one even strip; by time, each is as long as it took.
// The projection, the window and the range dragged out are in the address (`proj=time`,
// `zoom=<from>,<to>` in steps or in seconds of the run, `range=<request>-<request>`).
//
// It is drawn on canvases: a trace of thousands of steps is as many spans in each lane, which as
// elements take longer than a frame to lay out again at each turn of the wheel. Its gestures are
// deepseek-harness's trajectory timeline's
// (https://github.com/deepseek-ai/deepseek-harness/blob/39163dd30f703a79079e2d2446545cff3c453738/packages/client/ui-trajectory/src/client/TrajectoryTimeline.tsx#L358-L386):
// a drag selects a range, a click a step, the wheel zooms about the pointer; a zoomed window is
// panned by dragging it along the strip, or by the wheel across.

import { clock, h, route, sentence, setQuery, span } from "./h.js";
import { IN_FLIGHT, Steps } from "./steps.js";

/**
 * @typedef {object} OpColumns `trace/<caller>/ops`' `ops`: equal-length arrays, in the order the ops arrived
 * @property {number[]} op
 * @property {string[]} kind
 * @property {Array<string | null>} name
 * @property {Array<string | null>} start_ns
 * @property {Array<string | null>} end_ns
 * @property {Array<string | null>} result
 * @property {boolean[]} failed
 * @typedef {object} Look what the timeline shows besides its own state
 * @property {Steps | null} steps
 * @property {OpColumns | null} ops
 * @property {number | null} thread the one thread shown, by root
 * @property {Map<number, number> | null} hits the search's matches in each step, by request
 * @property {string | null} sel the selection's address
 * @property {number | null} now the run's clock now, in seconds, while it is live
 * @typedef {{ from: number, to: number }} Range a stretch of steps, by their first and last requests
 * @typedef {{ slot: number, child: number | null }} Tool a step that called tools, and the slot of the step that sends their outputs
 * @typedef {{ k: "step", slot: number } | { k: "tools", tool: Tool } | { k: "op", n: number }} Under what is under the pointer
 */

// The fewest steps, and the shortest time in seconds, that a zoomed window holds.
const MIN_STEPS = 4;
const MIN_SECONDS = 0.02;
// How long the pointer rests on a span before what it is shows, in ms.
const HOVER_MS = 300;
// How far the pointer moves, in px, before a press is a drag rather than a click.
const DRAG_PX = 4;
// The least space between two labels of the axis, in px.
const TICK_PX = 90;
// How much a wheel's notch zooms: the window scales by e to the notch's pixels times this.
const ZOOM_PER_PX = 0.0015;
// The steps of a nice axis, in seconds and in steps.
const SECOND_TICKS = [0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200];
// The least width of a span that a mark of its own is drawn for, in px; narrower ones blend.
const MARK_PX = 3;

// The style's colours that the canvases draw with, and its face and size of their text.
const COLOURS = ["--panel", "--select", "--line", "--line-soft", "--edge", "--dim", "--text", "--accent", "--model", "--runner", "--judge", "--agent", "--live", "--fail-bar", "--mark-bar"];
const FACES = ["--mono", "--fs-mono"];

/** @param {number} value @param {number} low @param {number} high */
const clamp = (value, low, high) => Math.min(high, Math.max(low, value));

export class Timeline {
  // `pick(request)` selects a step; `ranged()` is called when the range changes.
  /** @param {() => Look} look @param {(request: number) => void} pick @param {() => void} ranged */
  constructor(look, pick, ranged) {
    this.look = look;
    this.pick = pick;
    this.ranged = ranged;
    const { query } = route();
    /** @type {"step" | "time"} */
    this.proj = query.get("proj") === "time" ? "time" : "step";
    /** @type {[number, number] | null} the window in view, of the projection's domain; null the whole */
    this.zoom = null;
    const zoom = (query.get("zoom") || "").split(",").map(Number);
    if (zoom.length === 2 && zoom.every(Number.isFinite) && zoom[0] < zoom[1]) this.zoom = /** @type {[number, number]} */ (zoom);
    const range = /^(\d+)-(\d+)$/.exec(query.get("range") || "");
    /** @type {Range | null} */
    this.range = range && Number(range[1]) <= Number(range[2]) ? { from: Number(range[1]), to: Number(range[2]) } : null;
    /** @type {{ x: number, slots: number[] } | null} a drag being made, from `x`, in px */
    this.dragging = null;
    // The axis's unit, which a click switches.
    this.unit = h("button", { class: "tl-unit", onclick: () => this.project(this.proj === "step" ? "time" : "step") });
    this.labels = h("div", { class: "tl-labels" }, h("div", { class: "tl-lane" }, "Model"), h("div", { class: "tl-lane" }, "Tools"), h("div", { class: "tl-lane" }, "Ops"), h("div", { class: "tl-axis" }, this.unit));
    this.canvas = /** @type {HTMLCanvasElement} */ (h("canvas", { class: "tl-lanes" }));
    this.stripCanvas = /** @type {HTMLCanvasElement} */ (h("canvas", { class: "tl-strip", title: "The whole trace: drag the window to pan, double-click to see all of it" }));
    this.tip = h("div", { class: "tl-tip", hidden: true });
    // A colour token is `light-dark(…)`, which a canvas refuses: an element in the colour of each
    // gives it as the scheme shown resolves it.
    this.inks = h("div", { hidden: true }, COLOURS.map((name) => h("span", { style: { color: `var(${name})` } })));
    this.node = h(
      "div",
      { class: "timeline panel" },
      this.labels,
      h("div", { class: "tl-tracks" }, this.canvas, this.tip),
      h("div", { class: "tl-labels" }, h("div", { class: "tl-strip-label dim" }, "All")),
      this.stripCanvas,
      this.inks,
    );
    /** @type {number | undefined} */
    this.frame = undefined;
    /** @type {number | undefined} */
    this.hovering = undefined;
    /** @type {number | undefined} */
    this.writing = undefined;
    // The last drawing's slots, kept for the pointer to find spans in.
    this.slots = this.layout(null, null);
    new ResizeObserver(() => this.draw()).observe(this.node);
    // Another colour scheme changes the colours, which only a new drawing reads.
    matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => this.draw());
    new MutationObserver(() => this.draw()).observe(document.documentElement, { attributes: true });
    this.listen();
  }

  // ---- the steps as slots, in request order, and where each is in either projection

  /** @param {Steps | null} steps @param {number | null} thread */
  layout(steps, thread) {
    /** @type {number[]} the row of each slot */
    const rows = [];
    if (steps) for (let s = 0; s < steps.length; s++) if (thread === null || (steps.cols.thread[s] ?? IN_FLIGHT) === thread) rows.push(s);
    const n = rows.length;
    const [start, firstByte, firstToken, end, reach] = [0, 0, 0, 0, 0].map(() => new Float64Array(n));
    /** @type {Map<number, number>} the slot of each request shown */
    const slotOf = new Map();
    const now = this.look().now;
    let latest = -Infinity;
    for (let i = 0; i < n; i++) {
      const s = rows[i];
      const c = /** @type {Steps} */ (steps).cols;
      const t = (/** @type {string | null} */ ns) => (ns === null ? NaN : Number(ns) / 1e9);
      start[i] = t(c.start_ns[s]);
      firstByte[i] = t(c.first_byte_ns[s]);
      firstToken[i] = t(c.first_token_ns[s]);
      end[i] = t(c.end_ns[s]);
      // A step in flight reaches to now; one with no start known sits where the one before it
      // started.
      if (Number.isNaN(start[i])) start[i] = Number.isFinite(latest) ? latest : 0;
      if (Number.isNaN(end[i])) end[i] = /** @type {Steps} */ (steps).inFlight(s) && now !== null ? Math.max(now, start[i]) : start[i];
      latest = Math.max(latest, start[i]);
      // The latest start up to each slot, which only grows, so that a time's slot is found by
      // halving.
      reach[i] = latest;
      slotOf.set(c.request[s], i);
    }
    /** @type {Array<{ slot: number, child: number | null }>} each step that called tools, and the slot of the step that sends their outputs */
    const tools = [];
    if (steps) {
      const c = steps.cols;
      /** @type {Map<number, number>} the first child of each request shown */
      const child = new Map();
      for (let i = 0; i < n; i++) {
        const parent = c.parent[rows[i]];
        if (parent !== null && !child.has(parent)) child.set(parent, i);
      }
      for (let i = 0; i < n; i++) if ((c.calls[rows[i]] || []).length) tools.push({ slot: i, child: child.get(c.request[rows[i]]) ?? null });
    }
    let [first, last] = n ? [Infinity, -Infinity] : [0, 0];
    for (let i = 0; i < n; i++) (first = Math.min(first, start[i])), (last = Math.max(last, end[i]));
    return { rows, n, start, firstByte, firstToken, end, reach, slotOf, tools, first, last: Math.max(last, first + MIN_SECONDS) };
  }

  // The whole of the projection's domain: steps from 0, or seconds of the run.
  whole() {
    const l = this.slots;
    return this.proj === "step" ? [0, Math.max(l.n, MIN_STEPS)] : [l.first, l.last];
  }

  // The window in view, of the domain.
  view() {
    const [a, b] = this.whole();
    if (!this.zoom) return [a, b];
    const least = this.proj === "step" ? MIN_STEPS : MIN_SECONDS;
    const width = clamp(this.zoom[1] - this.zoom[0], Math.min(least, b - a), b - a);
    const from = clamp(this.zoom[0], a, b - width);
    return [from, from + width];
  }

  // Where a time of the run's clock falls among the slots, by step: within the slot of the
  // step that started last before it, as far along as it is to the next start.
  /** @param {number} t */
  stepAt(t) {
    const l = this.slots;
    if (!l.n || t <= l.reach[0]) return 0;
    let low = 0;
    let high = l.n - 1;
    while (low < high) {
      const mid = (low + high + 1) >> 1;
      if (l.reach[mid] <= t) low = mid;
      else high = mid - 1;
    }
    const next = low + 1 < l.n ? l.reach[low + 1] : Math.max(l.end[low], l.reach[low]);
    return low + (next > l.reach[low] ? clamp((t - l.reach[low]) / (next - l.reach[low]), 0, 1) : 1);
  }

  // A point of the step domain on the run's clock.
  /** @param {number} at */
  timeAt(at) {
    const l = this.slots;
    if (!l.n) return 0;
    const i = clamp(Math.floor(at), 0, l.n - 1);
    const next = i + 1 < l.n ? l.reach[i + 1] : Math.max(l.end[i], l.reach[i]);
    return l.reach[i] + clamp(at - i, 0, 1) * (next - l.reach[i]);
  }

  // A time of the run's clock in the projection's domain.
  /** @param {number} t */
  at(t) {
    return this.proj === "time" ? t : this.stepAt(t);
  }

  // The spans of slot `i` in the domain: its start, first byte, first token and end. By step a
  // span fills its slot, each part as long a share of it as of the step's time.
  /** @param {number} i */
  spanOf(i) {
    const l = this.slots;
    const [start, end] = [l.start[i], l.end[i]];
    if (this.proj === "time") return [start, l.firstByte[i], l.firstToken[i], end];
    const share = (/** @type {number} */ t) => (Number.isNaN(t) ? NaN : i + (end > start ? clamp((t - start) / (end - start), 0, 1) : 0));
    return [i, share(l.firstByte[i]), share(l.firstToken[i]), i + 1];
  }

  // The span of a step's tools: from its end to its child's start; by step, from the middle of
  // its slot to the middle of the next, as the steps of other threads between it and its child
  // would put a span over theirs.
  /** @param {Tool} tool */
  toolSpan(tool) {
    const l = this.slots;
    if (this.proj === "time") {
      const until = tool.child === null ? this.look().now ?? l.end[tool.slot] : l.start[tool.child];
      return [l.end[tool.slot], Math.max(until, l.end[tool.slot])];
    }
    return [tool.slot + 0.5, tool.child === null ? tool.slot + 1 : tool.slot + 1.5];
  }

  // The span of op `n` in the domain; an op that hasn't ended reaches to now.
  /** @param {OpColumns} ops @param {number} n */
  opSpan(ops, n) {
    const start = Number(ops.start_ns[n]) / 1e9;
    const ended = ops.end_ns[n];
    const end = ended === null ? this.look().now ?? start : Number(ended) / 1e9;
    return [this.at(start), this.at(Math.max(end, start))];
  }

  // ---- the address

  keep() {
    clearTimeout(this.writing);
    // A turn of the wheel writes the address once it rests.
    this.writing = setTimeout(() => {
      const digits = this.proj === "step" ? 2 : 3;
      setQuery({ proj: this.proj === "time" ? "time" : null, zoom: this.zoom ? this.zoom.map((v) => v.toFixed(digits)).join(",") : null, range: this.range ? this.range.from + "-" + this.range.to : null });
    }, 150);
  }

  /** @param {"step" | "time"} proj */
  project(proj) {
    if (proj === this.proj) return;
    // The window keeps the steps it holds.
    const zoom = this.zoom ? this.view() : null;
    if (zoom && proj === "time") this.zoom = [this.timeAt(zoom[0]), this.timeAt(zoom[1])];
    if (zoom && proj === "step") this.zoom = [this.stepAt(zoom[0]), this.stepAt(zoom[1])];
    this.proj = proj;
    this.keep();
    this.draw();
  }

  // Shows the whole of another set of steps, as another thread's.
  reset() {
    this.zoom = null;
    this.keep();
  }

  clearRange() {
    if (!this.range) return false;
    this.range = null;
    this.keep();
    this.ranged();
    this.draw();
    return true;
  }

  // Brings the span of `request`'s step into view, if a zoomed window has it outside.
  /** @param {number} request */
  reveal(request) {
    const i = this.slots.slotOf.get(request);
    if (i === undefined || !this.zoom) return;
    const [a, b] = this.view();
    const [start, , , end] = this.spanOf(i);
    if (start >= a && end <= b) return;
    const width = b - a;
    this.zoom = [(start + end) / 2 - width / 2, (start + end) / 2 + width / 2];
    this.keep();
  }

  // ---- drawing

  // Draws again at the next frame, once however often it is asked.
  draw() {
    if (this.frame !== undefined) return;
    this.frame = requestAnimationFrame(() => {
      this.frame = undefined;
      this.paint();
    });
  }

  // Lays the steps out again, as the look gives them, and draws.
  take() {
    const look = this.look();
    this.slots = this.layout(look.steps, look.thread);
    this.draw();
  }

  paint() {
    if (!this.node.isConnected) return;
    const look = this.look();
    const style = getComputedStyle(this.node);
    /** @type {Record<string, string>} */
    const k = Object.fromEntries(FACES.map((name) => [name, style.getPropertyValue(name).trim()]));
    COLOURS.forEach((name, index) => (k[name] = getComputedStyle(this.inks.children[index]).color));
    this.unit.textContent = this.proj === "step" ? "Step" : "Time";
    this.unit.title = this.proj === "step" ? "By step: every step as wide as the next. Click for time, every step as long as it took" : "By time: every step as long as it took, on the run's clock. Click for steps, every step as wide as the next";
    const lanes = [...this.labels.children].map((lane) => {
      const box = /** @type {HTMLElement} */ (lane);
      return { top: box.offsetTop, height: box.offsetHeight };
    });
    const height = lanes[3].top + lanes[3].height;
    const width = this.canvas.parentElement?.clientWidth || 0;
    this.canvas.style.height = height + "px";
    const g = sized(this.canvas, width, height);
    const strip = sized(this.stripCanvas, width, this.stripCanvas.clientHeight);
    if (!g || !strip) return;
    const [a, b] = this.view();
    const x = (/** @type {number} */ d) => ((d - a) / (b - a)) * width;
    const l = this.slots;
    const c = look.steps?.cols;
    g.clearRect(0, 0, width, height);
    const [model, tools, ops, axis] = lanes;

    // Lane rules.
    g.fillStyle = k["--line-soft"];
    for (const lane of [model, tools]) g.fillRect(0, lane.top + lane.height - 1, width, 1);
    g.fillStyle = k["--line"];
    g.fillRect(0, axis.top, width, 1);

    // The range, behind everything.
    const range = this.rangeSpan();
    if (range) {
      g.fillStyle = k["--select"];
      g.fillRect(x(range[0]), 0, Math.max(1, x(range[1]) - x(range[0])), axis.top);
    }

    // Thread and turn marks: a rule through the model and tools lanes where a thread begins, a
    // tick where a turn does.
    if (c) {
      let lastThread = -Infinity;
      let lastTurn = -Infinity;
      for (let i = this.firstIn(a); i < l.n; i++) {
        const [start] = this.spanOf(i);
        if (start > b) break;
        const s = l.rows[i];
        const request = c.request[s];
        const px = Math.round(x(start));
        if (c.thread[s] === request && i > 0 && px - lastThread >= MARK_PX) {
          g.fillStyle = k["--line"];
          g.fillRect(px, model.top, 1, ops.top - model.top);
          lastThread = px;
        } else if (c.turn[s] === request && c.parent[s] !== null && px - lastTurn >= MARK_PX) {
          g.fillStyle = k["--edge"];
          g.fillRect(px, model.top, 1, model.height / 3);
          lastTurn = px;
        }
      }
    }

    // The model lane: a step's wait for its first byte, its wait for its first token and its
    // decoding; a failed step or one in flight is drawn whole.
    /** @type {Map<string, Array<[number, number]>>} the spans of each fill */
    const fills = new Map();
    const fill = (/** @type {string} */ key, /** @type {number} */ from, /** @type {number} */ to) => {
      let list = fills.get(key);
      if (!list) fills.set(key, (list = []));
      list.push([from, to]);
    };
    const perSlot = width / Math.max(1e-9, b - a);
    const gap = this.proj === "step" && perSlot >= 6 ? 1 : 0;
    if (look.steps && c) {
      for (let i = this.firstIn(a); i < l.n; i++) {
        const [start, firstByte, firstToken, end] = this.spanOf(i);
        if (start > b) break;
        if (end < a) continue;
        const [from, to] = [x(start), Math.max(x(end) - gap, x(start) + 1)];
        const tone = look.steps.tone(l.rows[i]);
        if (tone === "failed" || tone === "live") {
          fill(tone, from, to);
          continue;
        }
        const byte = Number.isNaN(firstByte) ? from : clamp(x(firstByte), from, to);
        const token = Number.isNaN(firstToken) ? byte : clamp(x(firstToken), byte, to);
        if (byte > from) fill("byte", from, byte);
        if (token > byte) fill("token", byte, token);
        fill("decode", token, to);
      }
    }
    const paintFills = (/** @type {CanvasRenderingContext2D} */ into, /** @type {number} */ top, /** @type {number} */ tall) => {
      /** @type {Record<string, [string, number]>} */
      const looks = { byte: [k["--edge"], 0.7], token: [k["--model"], 0.45], decode: [k["--model"], 1], failed: [k["--fail-bar"], 1], live: [k["--live"], 1] };
      for (const [key, spans] of fills) {
        into.globalAlpha = looks[key][1];
        into.fillStyle = looks[key][0];
        into.beginPath();
        for (const [from, to] of spans) into.rect(from, top, to - from, tall);
        into.fill();
      }
      into.globalAlpha = 1;
    };
    const inset = Math.max(2, Math.round(model.height / 5));
    paintFills(g, model.top + inset, model.height - 2 * inset);

    // Search matches: a bar along the top of each step that has some.
    if (look.hits && c) {
      g.fillStyle = k["--mark-bar"];
      for (const [request] of look.hits) {
        const i = l.slotOf.get(request);
        if (i === undefined) continue;
        const [start, , , end] = this.spanOf(i);
        if (end < a || start > b) continue;
        g.fillRect(x(start), model.top, Math.max(2, x(end) - x(start) - gap), inset - 1);
      }
    }

    // The tools lane, labelled by the calls where a span is wide enough.
    g.font = k["--fs-mono"] + " " + k["--mono"];
    g.textBaseline = "middle";
    const toolTop = tools.top + inset;
    const toolTall = tools.height - 2 * inset;
    for (const tool of l.tools) {
      const [start, end] = this.toolSpan(tool);
      if (end < a || start > b) continue;
      const [from, to] = [x(start) + gap, Math.max(x(end) - gap, x(start) + 1)];
      g.fillStyle = k["--runner"];
      g.globalAlpha = 0.55;
      g.fillRect(from, toolTop, to - from, toolTall);
      g.globalAlpha = 1;
      if (to - from > 40 && look.steps) {
        g.fillStyle = k["--text"];
        g.save();
        g.beginPath();
        g.rect(from, tools.top, to - from, tools.height);
        g.clip();
        g.fillText(look.steps.calls(l.rows[tool.slot]), Math.max(from, 0) + 3, tools.top + tools.height / 2);
        g.restore();
      }
    }

    // The ops lane: each op's span, or a line saying it has none.
    if (look.ops && !look.ops.op.length) {
      g.fillStyle = k["--dim"];
      g.fillText("None brokered for this caller", 3, ops.top + ops.height / 2);
    }
    if (look.ops) {
      const o = look.ops;
      for (let n = 0; n < o.op.length; n++) {
        const [start, end] = this.opSpan(o, n);
        if (end < a || start > b) continue;
        g.fillStyle = o.failed[n] ? k["--fail-bar"] : o.kind[n] === "call" ? k["--judge"] : o.kind[n] === "submit" ? k["--agent"] : k["--runner"];
        g.fillRect(x(start), ops.top + inset, Math.max(2, x(end) - x(start)), ops.height - 2 * inset);
      }
    }

    // What the selection concerns: its step, or its thread's steps, outlined in the accent.
    const lit = this.lit(look);
    if (lit.length) {
      g.strokeStyle = k["--accent"];
      g.lineWidth = 2;
      for (const i of lit) {
        const [start, , , end] = this.spanOf(i);
        if (end < a || start > b) continue;
        g.strokeRect(x(start) - 1, model.top + 1, Math.max(2, x(end) - x(start) - gap) + 2, model.height - 2);
      }
    }

    // Outside a range, the rest dims.
    if (range) {
      g.globalAlpha = 0.6;
      g.fillStyle = k["--panel"];
      g.fillRect(0, 0, Math.max(0, x(range[0])), axis.top);
      g.fillRect(x(range[1]), 0, width - x(range[1]), axis.top);
      g.globalAlpha = 1;
    }
    // The range being dragged.
    if (this.dragging && this.dragging.slots.length === 2) {
      const [from, to] = this.dragging.slots;
      g.fillStyle = k["--accent"];
      g.globalAlpha = 0.15;
      g.fillRect(Math.min(from, to), 0, Math.abs(to - from), axis.top);
      g.globalAlpha = 1;
    }

    // The axis: requests by step, the run's clock by time.
    g.fillStyle = k["--dim"];
    g.textBaseline = "middle";
    const ticks = this.ticks(a, b, width);
    for (const tick of ticks) {
      const px = x(tick.at);
      g.fillRect(px, axis.top, 1, 4);
      const text = tick.label;
      const w = g.measureText(text).width;
      g.fillText(text, clamp(px - w / 2, 0, width - w), axis.top + axis.height / 2 + 1);
    }

    // The strip: the whole trace, and the window in view.
    const [wa, wb] = this.whole();
    const sw = strip.canvas.clientWidth;
    const sh = strip.canvas.clientHeight;
    const sx = (/** @type {number} */ d) => ((d - wa) / (wb - wa)) * sw;
    strip.clearRect(0, 0, sw, sh);
    fills.clear();
    if (look.steps) {
      for (let i = 0; i < l.n; i++) {
        const [start, , , end] = this.spanOf(i);
        const tone = look.steps.tone(l.rows[i]);
        fill(tone === "failed" || tone === "live" ? tone : "decode", sx(start), Math.max(sx(end), sx(start) + 0.5));
      }
    }
    paintFills(strip, 3, sh - 6);
    if (range) {
      strip.fillStyle = k["--accent"];
      strip.globalAlpha = 0.3;
      strip.fillRect(sx(range[0]), 0, Math.max(1, sx(range[1]) - sx(range[0])), sh);
      strip.globalAlpha = 1;
    }
    if (this.zoom) {
      strip.globalAlpha = 0.6;
      strip.fillStyle = k["--panel"];
      strip.fillRect(0, 0, sx(a), sh);
      strip.fillRect(sx(b), 0, sw - sx(b), sh);
      strip.globalAlpha = 1;
      strip.strokeStyle = k["--accent"];
      strip.lineWidth = 1;
      strip.strokeRect(sx(a) + 0.5, 0.5, Math.max(2, sx(b) - sx(a)) - 1, sh - 1);
    }
  }

  // The first slot whose span can reach `from`: by step, the slot at it; by time, the first of
  // all, as a step that started long before may still be running.
  /** @param {number} from */
  firstIn(from) {
    if (this.proj === "step") return clamp(Math.floor(from), 0, this.slots.n);
    return 0;
  }

  // The slots that the selection concerns.
  /** @param {Look} look @returns {number[]} */
  lit(look) {
    const sel = look.sel;
    if (!sel || !look.steps) return [];
    if (sel.startsWith("t")) {
      const root = Number(sel.slice(1));
      return this.slots.rows.flatMap((s, i) => ((look.steps?.cols.thread[s] ?? IN_FLIGHT) === root ? [i] : []));
    }
    const i = this.slots.slotOf.get(Number(sel.split("/")[0]));
    return i === undefined ? [] : [i];
  }

  // The range in the domain, from its first step's start to its last step's end.
  rangeSpan() {
    if (!this.range) return null;
    const l = this.slots;
    const steps = this.look().steps;
    if (!steps) return null;
    let [from, to] = [Infinity, -Infinity];
    for (let i = 0; i < l.n; i++) {
      const request = steps.cols.request[l.rows[i]];
      if (request < this.range.from || request > this.range.to) continue;
      const [start, , , end] = this.spanOf(i);
      from = Math.min(from, start);
      to = Math.max(to, end);
    }
    return from <= to ? [from, to] : null;
  }

  /** @param {number} a @param {number} b @param {number} width @returns {Array<{ at: number, label: string }>} */
  ticks(a, b, width) {
    const room = Math.max(1, Math.floor(width / TICK_PX));
    const ticks = [];
    if (this.proj === "time") {
      const every = SECOND_TICKS.find((step) => (b - a) / step <= room) ?? (b - a) / room;
      for (let t = Math.ceil(a / every) * every; t <= b; t += every) ticks.push({ at: t, label: clock(t) });
      return ticks;
    }
    const c = this.look().steps?.cols;
    if (!c || !this.slots.n) return ticks;
    let every = 1;
    for (let scale = 1; (b - a) / every > room; scale *= 10) every = [1, 2, 5, 10].map((base) => base * scale).find((step) => (b - a) / step <= room) ?? 10 * scale;
    for (let i = Math.ceil(a / every) * every; i < Math.min(b, this.slots.n); i += every) ticks.push({ at: i + 0.5, label: "#" + c.request[this.slots.rows[i]] });
    return ticks;
  }

  // ---- what is under the pointer

  // The step, tool span or op under `px`, in the lane at `py`, as the hover says it.
  /** @param {number} px @param {number} py @returns {Under | null} */
  under(px, py) {
    const look = this.look();
    const l = this.slots;
    const lanes = [...this.labels.children].map((lane) => /** @type {HTMLElement} */ (lane));
    const lane = lanes.findIndex((box) => py >= box.offsetTop && py < box.offsetTop + box.offsetHeight);
    const width = this.canvas.clientWidth;
    const [a, b] = this.view();
    const d = a + (px / width) * (b - a);
    // Half a few pixels of slack, in the domain, for a span narrower than the pointer.
    const slack = (3 / width) * (b - a);
    if (!look.steps || lane < 0 || lane > 2) return null;
    if (lane === 0) {
      let best = -1;
      if (this.proj === "step") best = d >= 0 && d < l.n ? Math.floor(d) : -1;
      else for (let i = 0; i < l.n; i++) if (l.start[i] - slack <= d && d <= l.end[i] + slack) best = i;
      return best < 0 ? null : { k: "step", slot: best };
    }
    if (lane === 1) {
      const tool = l.tools.find((tool) => {
        const [start, end] = this.toolSpan(tool);
        return start - slack <= d && d <= end + slack;
      });
      return tool ? { k: "tools", tool } : null;
    }
    const o = look.ops;
    if (!o) return null;
    for (let n = o.op.length - 1; n >= 0; n--) {
      const [start, end] = this.opSpan(o, n);
      if (start - slack <= d && d <= end + slack) return { k: "op", n };
    }
    return null;
  }

  // What a hover says of what is under the pointer, a line each.
  /** @param {Under} what @returns {string[]} */
  said(what) {
    const look = this.look();
    const steps = /** @type {Steps} */ (look.steps);
    const l = this.slots;
    const c = steps.cols;
    const between = (/** @type {number} */ from, /** @type {number} */ to) => clock(from) + " → " + clock(to) + " · " + span(to - from);
    if (what.k === "step") {
      const s = l.rows[what.slot];
      const { at, of } = steps.place(s);
      const t = steps.times(s);
      const head = "#" + c.request[s] + (c.thread[s] === null ? " · in flight" : " · step " + at + " of " + of + " of thread " + c.thread[s]);
      if (t.start.value === null) return [head, "Its start unknown: " + t.start.why];
      const end = t.end.value ?? look.now;
      const lines = [head, end === null ? clock(t.start.value) + " →" : between(t.start.value, end) + (t.end.value === null ? " so far" : "")];
      const byte = t.firstByteAt.value;
      const token = t.firstTokenAt.value;
      if (byte !== null || token !== null) {
        const parts = [];
        if (byte !== null) parts.push("first byte " + span(byte - t.start.value));
        if (token !== null) parts.push("first token " + span(token - t.start.value));
        if (token !== null && t.end.value !== null) parts.push("decoding " + span(t.end.value - token));
        lines.push(sentence(parts.join(" · ")));
      } else lines.push("First byte and token: " + t.firstTokenAt.why);
      if (steps.tone(s) === "failed") lines.push("Failed: " + steps.why(s));
      return lines;
    }
    if (what.k === "tools") {
      const s = l.rows[what.tool.slot];
      const head = "The tools of #" + c.request[s] + ": " + steps.calls(s);
      if (what.tool.child === null) return [head, "From its end, " + clock(l.end[what.tool.slot]) + "; no later step sends their outputs yet"];
      const child = l.rows[what.tool.child];
      return [head, "#" + c.request[s] + "'s end to #" + c.request[child] + "'s start: " + between(l.end[what.tool.slot], l.start[what.tool.child]), "Spent on the tools and the harness"];
    }
    const o = /** @type {OpColumns} */ (look.ops);
    const n = what.n;
    const start = Number(o.start_ns[n]) / 1e9;
    const end = o.end_ns[n] === null ? null : Number(o.end_ns[n]) / 1e9;
    const name = o.kind[n].replace("lease_", "lease ") + (o.name[n] ? " · " + o.name[n] : "");
    const when = end === null ? clock(start) + " →, not ended" : end === start ? "At " + clock(start) : between(start, end);
    return ["Op " + o.op[n] + ": " + name, when, ...(o.result[n] ? [o.result[n] ?? ""] : [])];
  }

  // ---- gestures

  listen() {
    const canvas = this.canvas;
    const local = (/** @type {MouseEvent} */ event) => {
      const box = canvas.getBoundingClientRect();
      return { px: event.clientX - box.left, py: event.clientY - box.top, width: box.width };
    };
    const domainAt = (/** @type {number} */ px, /** @type {number} */ width) => {
      const [a, b] = this.view();
      return a + (clamp(px, 0, width) / width) * (b - a);
    };
    canvas.addEventListener("pointerdown", (event) => {
      if (event.button !== 0) return;
      canvas.setPointerCapture(event.pointerId);
      this.dragging = { x: local(event).px, slots: [] };
      this.hide();
    });
    canvas.addEventListener("pointermove", (event) => {
      const { px, py } = local(event);
      if (this.dragging) {
        if (Math.abs(px - this.dragging.x) >= DRAG_PX) {
          this.dragging.slots = [this.dragging.x, px];
          this.draw();
        }
        return;
      }
      this.hover(px, py, event.clientX, event.clientY);
    });
    canvas.addEventListener("pointerleave", () => this.hide());
    canvas.addEventListener("pointerup", (event) => {
      const drag = this.dragging;
      this.dragging = null;
      if (!drag) return;
      const { px, py, width } = local(event);
      if (drag.slots.length === 2) {
        this.select(domainAt(Math.min(drag.x, px), width), domainAt(Math.max(drag.x, px), width));
        return;
      }
      const what = this.under(px, py);
      const steps = this.look().steps;
      if (!what || !steps) return;
      // A step picked is the focus now, in place of the range.
      if (this.range) (this.range = null), this.keep(), this.ranged();
      if (what.k === "step") this.pick(steps.cols.request[this.slots.rows[what.slot]]);
      else if (what.k === "tools") this.pick(steps.cols.request[this.slots.rows[what.tool.slot]]);
      else {
        // An op is picked as the step that started last before it, whose tools sent it.
        const ops = /** @type {OpColumns} */ (this.look().ops);
        const slot = Math.min(this.slots.n - 1, Math.floor(this.stepAt(Number(ops.start_ns[what.n]) / 1e9)));
        if (slot >= 0) this.pick(steps.cols.request[this.slots.rows[slot]]);
      }
    });
    canvas.addEventListener("dblclick", () => this.clearRange());
    const wheel = (/** @type {WheelEvent} */ event, /** @type {HTMLCanvasElement} */ over, /** @type {boolean} */ whole) => {
      event.preventDefault();
      this.hide();
      const box = over.getBoundingClientRect();
      const [a, b] = this.view();
      const [wa, wb] = this.whole();
      // A notch in lines or pages is as many pixels as a line of the ledger, or a page of it.
      const scale = event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? box.width : 1;
      const across = event.shiftKey ? event.deltaY : event.deltaX;
      if (Math.abs(across) > Math.abs(event.deltaY) || event.shiftKey) {
        const by = ((across * scale) / box.width) * (b - a);
        this.zoom = [a + by, b + by];
      } else {
        const factor = Math.exp(event.deltaY * scale * ZOOM_PER_PX);
        const pivot = whole ? (a + b) / 2 : a + ((event.clientX - box.left) / box.width) * (b - a);
        const least = this.proj === "step" ? MIN_STEPS : MIN_SECONDS;
        const width = clamp((b - a) * factor, Math.min(least, wb - wa), wb - wa);
        const from = pivot - ((pivot - a) / (b - a)) * width;
        this.zoom = [from, from + width];
      }
      this.zoom = /** @type {[number, number]} */ (this.view());
      // The whole domain, give or take a rounding, is no zoom.
      if (this.zoom[1] - this.zoom[0] >= (wb - wa) * (1 - 1e-9)) this.zoom = null;
      this.keep();
      this.draw();
    };
    canvas.addEventListener("wheel", (event) => wheel(event, canvas, false), { passive: false });
    const strip = this.stripCanvas;
    strip.addEventListener("wheel", (event) => wheel(event, strip, true), { passive: false });
    // A press on the strip moves the window there, and a drag carries it along.
    strip.addEventListener("pointerdown", (event) => {
      if (event.button !== 0 || !this.zoom) return;
      strip.setPointerCapture(event.pointerId);
      const box = strip.getBoundingClientRect();
      const [wa, wb] = this.whole();
      const at = (/** @type {number} */ clientX) => wa + ((clientX - box.left) / box.width) * (wb - wa);
      const [a, b] = this.view();
      const grab = at(event.clientX);
      const offset = grab >= a && grab <= b ? grab - a : (b - a) / 2;
      const move = (/** @type {PointerEvent} */ moved) => {
        const from = at(moved.clientX) - offset;
        this.zoom = [from, from + (b - a)];
        this.zoom = /** @type {[number, number]} */ (this.view());
        this.keep();
        this.draw();
      };
      move(event);
      strip.addEventListener("pointermove", move);
      strip.addEventListener("pointerup", () => strip.removeEventListener("pointermove", move), { once: true });
    });
    strip.addEventListener("dblclick", () => {
      this.zoom = null;
      this.keep();
      this.draw();
    });
  }

  // Keeps as the range the steps whose spans meet the stretch from `from` to `to` of the domain.
  /** @param {number} from @param {number} to */
  select(from, to) {
    const steps = this.look().steps;
    const l = this.slots;
    if (!steps) return;
    let [first, last] = [Infinity, -Infinity];
    for (let i = 0; i < l.n; i++) {
      const [start, , , end] = this.spanOf(i);
      if (end < from || start > to) continue;
      const request = steps.cols.request[l.rows[i]];
      first = Math.min(first, request);
      last = Math.max(last, request);
    }
    this.range = first <= last ? { from: first, to: last } : null;
    this.keep();
    this.ranged();
    this.draw();
  }

  /** @param {number} px @param {number} py @param {number} clientX @param {number} clientY */
  hover(px, py, clientX, clientY) {
    clearTimeout(this.hovering);
    this.tip.hidden = true;
    this.hovering = setTimeout(() => {
      const what = this.under(px, py);
      if (!what) return;
      this.tip.replaceChildren(...this.said(what).map((line, at) => h("div", { class: at ? null : "head" }, line)));
      this.tip.hidden = false;
      const box = /** @type {HTMLElement} */ (this.tip.parentElement).getBoundingClientRect();
      const left = clamp(clientX - box.left + 12, 0, Math.max(0, box.width - this.tip.offsetWidth));
      this.tip.style.left = left + "px";
      this.tip.style.top = clientY - box.top + 14 + "px";
    }, HOVER_MS);
  }

  hide() {
    clearTimeout(this.hovering);
    this.tip.hidden = true;
  }
}

// `canvas`'s context, sized `width` by `height` CSS px at the screen's density; null for none.
/** @param {HTMLCanvasElement} canvas @param {number} width @param {number} height */
function sized(canvas, width, height) {
  const ratio = devicePixelRatio || 1;
  if (canvas.width !== Math.round(width * ratio) || canvas.height !== Math.round(height * ratio)) {
    canvas.width = Math.round(width * ratio);
    canvas.height = Math.round(height * ratio);
  }
  const g = canvas.getContext("2d");
  g?.setTransform(ratio, 0, 0, ratio, 0, 0);
  return g;
}
