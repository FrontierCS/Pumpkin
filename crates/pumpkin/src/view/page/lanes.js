// The lanes of a run's sandboxes over its time, and the inspector of one lane.

import { card, cardRows, clock, count, fill, h, plural, span as lasted, utc } from "./h.js";
import { laneRole } from "./talk.js";
import { laneMark } from "./failure.js";

/**
 * @typedef {import("./run.js").Run} Run
 * @typedef {import("./run.js").Lane} Lane
 * @typedef {import("./run.js").SandboxStart} SandboxStart
 * @typedef {import("./run.js").Lit} Lit
 * @typedef {import("./failure.js").Failure} Failure
 */

// The bounds of the characters of the lanes' names and places before the tracks, which the style
// turns into `--label` in its monospace.
const LABEL_MIN_CHARS = 18;
const LABEL_MAX_CHARS = 48;

/** What a call's lane calls, as words. @param {{ function: string } | "final"} of */
function calls(of) {
  return of === "final" ? "the final call" : "job function " + of.function;
}

// The lanes of the run's sandboxes, each from when the core asked for it until it was gone: its
// start faint, or in parts where the log timed them, its life solid, its calls or a lease's work on
// it, and a tick at each model answer. A sandbox that isn't gone runs to the last time the record
// knows, its end open.
/** @param {HTMLElement} node @param {Run} data @param {Map<string, Lane[]>} jobs @param {{ from: number | null, to: number | null, lit: Lit | null }} view @param {string | null} picked @param {(change: Record<string, string | null>) => void} change @param {(lane: Lane) => void} pick @param {(failure: Failure) => void} pickFailure */
export function drawLanes(node, data, jobs, view, picked, change, pick, pickFailure) {
  const times = data.times;
  if (!data.lanes || !times) {
    node.replaceChildren(h("p", { class: "empty" }, "The run's log doesn't record its start yet."));
    return;
  }
  const known = times.end ?? times.now ?? times.last;
  const span = data.lanes.reduce((last, lane) => Math.max(last, lane.sandbox.gone ?? 0), Math.max(known, 1));
  const at = (/** @type {number} */ seconds) => (100 * seconds) / span + "%";
  const width = (/** @type {number} */ from, /** @type {number} */ to) => Math.max(0.15, (100 * (to - from)) / span) + "%";
  const ticks = [];
  for (let index = 0; index <= 4; index++) ticks.push(h("span", { style: { left: index * 25 + "%" } }, clock((span * index) / 4)));
  // Each failure's mark, by the lane it is drawn on: its subject's, a call on a resident sandbox
  // on that sandbox's; the failure the run ended with among them once, and on the axis if its
  // subject has no lane.
  /** @type {Map<string | null, Failure[]>} */
  const marks = new Map();
  const ended = data.failure && !data.failures.some((failure) => failure.mark.record === data.failure?.mark.record) ? [data.failure] : [];
  for (const failure of [...data.failures, ...ended]) {
    if (failure.mark.at === null) continue;
    const of = data.lanes.find((lane) => lane.lane === failure.mark.lane);
    const on = of ? (of.service ?? of.lane) : null;
    marks.set(on, [...(marks.get(on) || []), failure]);
  }
  const marksOn = (/** @type {string | null} */ lane) => (marks.get(lane) || []).map((failure) => laneMark(failure, at(failure.mark.at ?? 0), pickFailure));
  // A lane's name says its role and the role's place; the lane's own place, which can name its
  // GPU, is in its title and the inspector.
  const sandboxes = data.lanes.filter((lane) => !lane.service);
  const labels = sandboxes.map((lane) => {
    const role = laneRole(lane, data);
    const name = lane.lane + (role && role !== lane.lane ? " · " + role : "");
    const placed = data.talk && data.talk.roles.find((r) => r.name === role);
    return { name, place: (placed && placed.place) || lane.sandbox.place };
  });
  const widest = Math.max(...labels.map(({ name, place }) => name.length + 1 + place.length));
  node.style.setProperty("--label-chars", String(Math.min(LABEL_MAX_CHARS, Math.max(LABEL_MIN_CHARS, widest))));
  const across = (/** @type {number} */ fraction) => "calc(var(--label) + (100% - var(--label)) * " + fraction + ")";
  const rows = sandboxes.map((lane, index) => {
    const { asked, started, gone } = lane.sandbox;
    const end = gone ?? known;
    const parts = [];
    // What a bar's card says first: whose lane it is, and where it runs.
    /** @type {Array<[string, string]>} */
    const where = [
      ["Lane", labels[index].name],
      ["On", lane.sandbox.place],
    ];
    const start = lane.sandbox.start;
    if (started !== null) {
      parts.push(start ? startBar(lane, start, end, where, { at, width }) : card(h("div", { class: "span starting", style: { left: at(asked), width: width(asked, started) } }), () => cardRows([...where, ["Asked for", clock(asked)], ["Starting", lasted(started - asked)]])));
      parts.push(
        card(h("div", { class: "span" + (gone === null ? " open" : ""), style: { left: at(started), width: width(started, end) } }), () =>
          cardRows([...where, ["Started", clock(started)], [gone === null ? "Running" : "Ran", lasted(end - started)], gone === null ? null : ["Gone", clock(gone)], ["", lane.said]]),
        ),
      );
    } else {
      parts.push(
        start
          ? startBar(lane, start, end, where, { at, width })
          : card(h("div", { class: "span starting" + (gone === null ? " open" : ""), style: { left: at(asked), width: width(asked, end) } }), () =>
              cardRows([...where, ["Asked for", clock(asked)], [gone === null ? "Starting" : "Never started", lasted(end - asked)], ["", lane.said]]),
            ),
      );
    }
    for (const work of lane.work || []) {
      const failed = work.end && (work.end.failed || (work.end.exited && work.end.exited.code !== 0) || typeof work.end === "string");
      parts.push(card(h("div", { class: "work" + (failed ? " failed" : ""), style: { left: at(work.from), width: width(work.from, work.to ?? known) } }), () => cardRows([[work.kind, work.label], ["From", clock(work.from)], [work.to == null ? "So far" : "Took", lasted((work.to ?? known) - work.from)]])));
    }
    const callsHere = jobs.get(lane.lane) || [];
    for (const job of callsHere) {
      const title = job.lane + (job.of ? ", " + calls(job.of) : "") + (job.holder ? ", from " + job.holder : "");
      const { asked, gone } = job.sandbox;
      const lit = view.lit?.here != null && view.lit.lanes.has(job.lane);
      parts.push(h("button", { class: "work call" + (lit || picked === job.lane ? " lit" : ""), style: { left: at(asked), width: width(asked, gone ?? known) }, title, onclick: () => pick(job) }));
    }
    for (const caller of [lane, ...callsHere]) {
      const failed = new Set(caller.failed || []);
      for (const answered of caller.answered || []) parts.push(h("div", { class: "tick" + (failed.has(answered) ? " failed" : ""), style: { left: at(answered) } }));
    }
    parts.push(...marksOn(lane.lane));
    const role = laneRole(lane, data);
    const { name, place } = labels[index];
    const title = lane.lane + (role ? " of role " + role : "") + (lane.of ? ", " + calls(lane.of) : "") + (lane.grant ? ", grant " + lane.grant : "") + ", on " + lane.sandbox.place;
    const lit = view.lit;
    const classes = ["lane", lane.kind, picked === lane.lane ? "on" : "", lit && !lit.lanes.has(lane.lane) ? "dim" : "", lit && lit.here !== null && lit.lanes.has(lane.lane) ? "lit" : ""];
    return h("div", { class: classes.join(" ") }, h("div", { class: "name", title, onclick: () => pick(lane) }, name, " ", h("span", { class: "place" }, place)), h("div", { class: "track" }, parts));
  });
  const axis = h("div", { class: "axis" }, h("div", { class: "ticks" }, ticks, marksOn(null)));
  const guides = ticks.map((_, index) => h("div", { class: "guide", style: { left: across(index / 4) } }));
  const tracks = h("div", { class: "tracks" }, guides, rows);
  if (times.now !== null) tracks.append(h("div", { class: "now", title: "Now", style: { left: across(times.now / span) } }));
  if (view.lit && view.lit.here !== null) tracks.append(h("div", { class: "here", style: { left: across(view.lit.here / span) } }));
  if (view.from !== null && view.to !== null) {
    tracks.append(h("div", { class: "window", style: { left: across(view.from / span), width: "calc((100% - var(--label)) * " + (view.to - view.from) / span + ")" } }));
  }
  // A drag across the tracks keeps the records of that stretch of time.
  tracks.addEventListener("mousedown", (down) => {
    const box = tracks.getBoundingClientRect();
    const labelPx = parseFloat(getComputedStyle(axis).paddingLeft);
    const left = box.left + labelPx;
    const time = (/** @type {number} */ x) => Math.max(0, Math.min(span, ((x - left) / (box.width - labelPx)) * span));
    if (down.clientX < left) return;
    const window = h("div", { class: "window" });
    tracks.append(window);
    const move = (/** @type {MouseEvent} */ event) => {
      const [a, b] = [down.clientX, event.clientX].sort((x, y) => x - y);
      window.style.left = a - box.left + "px";
      window.style.width = b - a + "px";
    };
    const up = (/** @type {MouseEvent} */ event) => {
      removeEventListener("mousemove", move);
      removeEventListener("mouseup", up);
      if (Math.abs(event.clientX - down.clientX) < 4) return window.remove();
      const [from, to] = [time(down.clientX), time(event.clientX)].sort((x, y) => x - y);
      change({ from: from.toFixed(2), to: to.toFixed(2), sel: null });
    };
    addEventListener("mousemove", move);
    addEventListener("mouseup", up);
  });
  node.replaceChildren(axis, tracks);
}

// The start of a sandbox whose log timed it: one bar from when the core asked for it until it
// started, or until `end` while it hasn't. Its parts are shades of the lane's colour: waiting for
// the create, the create, meeting the gateway and setting up. The lanes have no zoom, so the bar
// can be a pixel wide, and a phase of the create narrower still: one card for the whole bar lists
// every part and phase.
/** @param {Lane} lane @param {SandboxStart} start @param {number} end @param {Array<[string, string]>} where @param {{ at: (seconds: number) => string, width: (from: number, to: number) => string }} scale */
function startBar(lane, start, end, where, scale) {
  const { asked, started, gone } = lane.sandbox;
  const { began, created, met, phases, more_phases } = start;
  const until = started ?? end;
  // Before it starts, the log gives its last part no end: the part runs to the bar's end, and its
  // row in the card says until when.
  const open = started === null;
  const rest = gone === null ? " so far" : " until gone";
  const meeting = met ?? until;
  // Where `seconds` falls in the bar, in percent of its width. A start that took no time has
  // parts of no width.
  const share = (/** @type {number} */ seconds) => (until > asked ? (100 * (seconds - asked)) / (until - asked) : 0);
  const piece = (/** @type {string} */ kind, /** @type {number} */ from, /** @type {number} */ to) => h("div", { class: kind, style: { left: share(from) + "%", width: share(to) - share(from) + "%" } });
  const bar = h(
    "div",
    { class: "span start" + (open && gone === null ? " open" : ""), style: { left: scale.at(asked), width: scale.width(asked, until) } },
    piece("part waiting", asked, began),
    piece("part creating", began, created),
    piece("part meeting", created, meeting),
    met === null ? null : piece("part setting-up", met, until),
  );
  return card(bar, () =>
    cardRows([
      ...where,
      ["Asked for", clock(asked)],
      ["Waiting", lasted(began - asked)],
      ["Creating", lasted(created - began)],
      ...phases.map((phase) => /** @type {[string, string]} */ (["· " + phase.name, phaseSaid(phase)])),
      more_phases ? ["· …", morePhasesSaid(more_phases)] : null,
      ["Meeting", lasted(meeting - created) + (open && met === null ? rest : "")],
      met === null ? null : ["Setting up", lasted(until - met) + (open ? rest : "")],
      started === null ? null : ["Started", clock(started)],
      [started === null && gone !== null ? "Never started" : "Starting", lasted(until - asked)],
      started === null ? ["", lane.said] : null,
    ]),
  );
}

// A phase of a start as the card and the inspector both say it: how long it took, and when it
// began.
/** @param {SandboxStart["phases"][number]} phase */
function phaseSaid(phase) {
  return lasted(phase.to - phase.from) + " from " + clock(phase.from);
}

// The phases of a start that the reply leaves out, as the card and the inspector both say them.
/** @param {number} count */
function morePhasesSaid(count) {
  return plural(count, "more phase") + ", which the page leaves out";
}

/** @param {Lane} lane @param {HTMLElement} inspector @param {(lane: string, time: number) => void} nearest @param {Run} data @param {HTMLElement} callList @param {(failure: Failure) => HTMLElement} failureOf */
export function inspectLane(lane, inspector, nearest, data, callList, failureOf) {
  const { place, asked, started, gone, deleted, start } = lane.sandbox;
  const at = (/** @type {number | null} */ seconds, said = "") => h("dd", { title: utc(data.times && data.times.started_unix, seconds) }, clock(seconds) + said);
  const work = (lane.work || []).map((w) =>
    h("li", {}, h("button", { class: "mono", title: "Select the record nearest the start of this", onclick: () => nearest(lane.lane, w.from) }, clock(w.from) + "–" + clock(w.to) + " " + w.kind + " " + w.label + "  " + (w.end ? JSON.stringify(w.end) : "running"))),
  );
  /** @type {Array<[string, string | null | undefined]>} */
  const named = [
    ["Role", lane.role],
    ["Sandbox", lane.service],
    ["Calls", lane.of && calls(lane.of)],
    ["Grant", lane.grant],
    ["Held by", lane.holder],
  ];
  const answers = lane.answers || { count: 0, failed: 0 };
  fill(
    inspector,
    h("h3", {}, h("span", { class: "kind " + lane.kind }, lane.kind), " ", lane.lane),
    h("p", { class: "said" }, lane.said),
    lane.failure ? failureOf(lane.failure) : null,
    h(
      "dl",
      {},
      named.filter(([, value]) => value).flatMap(([name, value]) => [h("dt", {}, name), h("dd", {}, value)]),
      h("dt", {}, "Place"),
      h("dd", {}, place),
      h("dt", {}, "Asked"),
      at(asked),
      start
        ? [
            h("dt", {}, "Create began"),
            at(start.began),
            ...start.phases.flatMap((phase) => [h("dt", {}, "· " + phase.name), h("dd", {}, phaseSaid(phase))]),
            ...(start.more_phases ? [h("dt", {}, "· …"), h("dd", {}, morePhasesSaid(start.more_phases))] : []),
            h("dt", {}, "Created"),
            at(start.created, ", " + lasted(start.created - start.began) + " after the create began"),
            h("dt", {}, "Met"),
            start.met === null ? at(null) : at(start.met, ", " + lasted(start.met - start.created) + " after it was created"),
          ]
        : null,
      h("dt", {}, "Started"),
      at(started),
      h("dt", {}, "Gone"),
      at(gone),
      h("dt", {}, "Deleted"),
      h("dd", {}, deleted === null ? "Not recorded" : String(deleted)),
      h("dt", {}, "Model answers"),
      h("dd", {}, count(answers.count, "ans") + (answers.failed ? ", " + answers.failed.toLocaleString("en-US") + " failed" : "")),
    ),
    work.length ? h("ul", {}, work) : null,
    lane.lifetime === "work_phase" ? callList : null,
  );
}
