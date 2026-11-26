// A run: its verdict and the command that runs it again, its sandboxes' lanes, who talks to whom,
// its log as a list that draws only the rows in view, and the inspector of what is selected.
//
// The page reads the log from its start in windows (`/log?at=<byte>`), up to the byte that the
// run's reply was read to. While the run is live, the page then polls from where the last read
// ended (`/since?log=<byte>`). A reply with the run replaces it, and appends the records it holds.
// A reply of only `end` and `live` is of a run that ended with no record past those read. A
// refusal of the byte, or a log shorter than it, means the log started over. The page then reads
// the run again from its start.

import { byId, bytes, card, cardRows, clock, count, fill, follow, get, grip, h, plural, POLL_MS, post, Refused, route, sentence, setKeys, setQuery, show, span, tone, typing, utc } from "./h.js";
import { drawKinds, folded, holds, lanesOf, pastWindow, recordRow, saidOf } from "./records.js";
import { drawResult } from "./returned.js";
import { RowList } from "./rows.js";
import { jsonText, payloadLink, payloadOf } from "./payload.js";
import { drawTalk, laneRole } from "./talk.js";
import { failureBox, failureParts } from "./failure.js";
import { drawLanes, inspectLane } from "./lanes.js";

/**
 * @typedef {object} Run `/api/runs/<id>`
 * @property {string} id
 * @property {string | null} task
 * @property {boolean} live
 * @property {string} state
 * @property {boolean} has_result
 * @property {string} verdict
 * @property {{ name: string, time: string, bound: string } | null} [stage] the stage of a run that goes on, as `ls` shows it: its name, how long it has lasted, and ` of ` how long it may, or nothing
 * @property {{ values: { name: string, value: any }[] } | null} result the values a run that finished with a result shows, in the order the body returned them
 * @property {Failure | null} failure how a run that ended without a result ended; null for a run with a result, one that is live, and one that is lost
 * @property {Failure[]} failures each failure the log states, of a call, an agent, a lease, a shared sandbox or a model request
 * @property {{ command: string, error?: undefined } | { error: string, command?: undefined } | undefined} again
 * @property {string | null} other_build
 * @property {Times | null} times
 * @property {Lane[] | null} lanes
 * @property {Talk | null} talk
 * @property {ModelUse | null} model
 * @property {Cost | null} cost null if nothing of it has a price
 * @property {Driver | null} driver null while the run is live, if its record keeps none, or if the record can't be read, which `summary` says
 * @property {{ count: number, said: string } | null} unread
 * @property {string | null} summary what `pumpkin summary` prints of it
 * @property {number} end the byte of the log that the run was read to
 * @property {number} log_epoch how many times the log started over since the view started
 * @typedef {object} Times seconds since the run started, but `started_unix`
 * @property {number} started_unix
 * @property {number} hard_end
 * @property {number} last
 * @property {number | null} end
 * @property {number | null} now
 * @typedef {object} Cost what the run cost so far, at the prices its record keeps
 * @property {string} shown the whole, as `ls`'s Cost column shows it: `$1.13`, `$1.13+`
 * @property {string} said what the summary's cost line says of it, after `cost: `
 * @typedef {object} Driver what the run's driver used, in the words of the summary's `driver:` line
 * @property {string} shown the driver's CPU time
 * @property {Array<[string, string]>} rows each figure by its name, the job's after the driver's
 * @typedef {object} ModelUse
 * @property {number} requests
 * @property {Array<{ code: string, count: number }>} failed
 * @property {number} not_2xx how many were not forwarded whole with a 2xx status
 * @property {Array<{ request: number, since: number }>} in_flight each with when it went
 * @property {{ median: number | null, count: number }} took
 * @property {{ median: number | null, count: number }} first_token
 * @property {{ input: number | null, output: number | null, cached: number | null }} tokens
 * @property {string[]} dropped_headers the request headers the route left behind, the first by name
 * @property {boolean} more_dropped_headers whether it left more than `dropped_headers`
 * @typedef {object} Lane a sandbox or a call on a resident sandbox
 * @property {string} lane
 * @property {string} [caller] the caller that the lane's trace is of, `agent-<n>` or `job-<n>`; none for a lease
 * @property {"agent" | "target" | "runner"} kind
 * @property {"work_phase"} [lifetime]
 * @property {string} [service] the resident sandbox that ran this job
 * @property {string | null} [role]
 * @property {{ function: string } | "final"} [of] whose body a call's lane calls: a job function's, by its name, or the final call's
 * @property {string | null} [grant] a lease's session
 * @property {string} [holder]
 * @property {{ place: string, asked: number, started: number | null, gone: number | null, deleted: boolean | null, start?: SandboxStart }} sandbox
 * @property {string} said
 * @property {number[]} [answered] the times of its model answers, at most 500 of them, to draw
 * @property {number[]} [failed] those of `answered` that stand for a failed answer
 * @property {{ count: number, failed: number }} [answers] how many model answers it has, and failed
 * @property {Work[] | null} [work]
 * @property {Failure | null} [failure] the failure that the lane's sandbox, call or lease ended with
 * @typedef {object} SandboxStart a sandbox's start as the runtime timed it, in seconds since the run started
 * @property {number} began when its provider's create began
 * @property {number} created when the create returned
 * @property {number | null} met when its session met the gateway, once the log says so
 * @property {{ name: string, from: number, to: number }[]} phases the first steps of the create that its provider timed, which may overlap or leave gaps
 * @property {number} more_phases how many more steps it timed, which the reply leaves out
 * @typedef {{ kind: string, label: string, from: number, to: number | null, end: any }} Work
 * @typedef {{ roles: Role[], grants?: Grant[], edges: Edge[] }} Talk
 * @typedef {{ from: string, to: string, kind: string }} Grant a grant of the IR: its holder and the role it reaches
 * @typedef {{ name: string, kind: string | null, lifetime?: "work_phase" | "per_call", place: string | null, routed: boolean, final_target: boolean, routes: Route[] | "unknown" }} Role
 * @typedef {{ api: string, upstream: string, models: Record<string, string> }} Route an entry of a role's model route: each model by the name the upstream takes it as
 * @typedef {{ from: string, to: string, kind: string, count: number, bytes: number | null, back_bytes: number | null }} Edge
 * @typedef {import("./records.js").LogRecord} LogRecord
 * @typedef {import("./records.js").Window} Window
 * @typedef {{ end: number, live: boolean, run?: Run, records?: LogRecord[], next?: number }} Since `/since`
 * @typedef {import("./records.js").Item} Item
 * @typedef {import("./failure.js").Failure} Failure
 */

// The bounds of the log's columns of who and of kind, in characters.
const [WHO_MIN_CHARS, WHO_MAX_CHARS, KIND_MIN_CHARS, KIND_MAX_CHARS] = [8, 28, 8, 32];
// How many records the log's list is drawn again after while the log is read from its start.
const DRAW_EVERY_RECORDS = 2000;
// How often a cancel under way is read.
const CANCEL_POLL_MS = 1000;

// ---- the run's page

/** @param {string} id */
export async function runView(id) {
  const base = "/api/runs/" + encodeURIComponent(id);
  // `cursor` is where the log is read (`Reading`). `open` holds the first record's byte of each
  // fold opened. `shown` holds the records that the address's filters keep. `selected` is the
  // `sel` last drawn.
  /** @type {{ data: Run | null, records: LogRecord[], cursor: number, caught: boolean, drawnAt: number, open: Set<number>, kinds: string, shown: LogRecord[], selected: string | null | undefined, inspected: boolean }} */
  const state = { data: null, records: [], cursor: 0, caught: false, drawnAt: 0, open: new Set(), kinds: "", shown: [], selected: undefined, inspected: false };
  const { query } = route();
  const counted = h("span", { class: "counted" });
  const all = h("span", {});
  const kinds = /** @type {HTMLSelectElement} */ (h("select", { title: "Show the records of one kind" }));
  const search = /** @type {HTMLInputElement} */ (h("input", { type: "search", class: "search", placeholder: "Filter records", title: "Keep the records that hold this text (/ to type, Esc to leave the box)", value: query.get("q") || "" }));
  const verdict = h("div", { class: "verdict" });
  const result = h("div", { class: "result" });
  const failed = h("div", { class: "failure" });
  const facts = h("div", { class: "facts" });
  const again = h("div", { class: "again" });
  const traces = h("div", { class: "traces" });
  const cancelBox = h("div", { class: "cancel" });
  const unread = h("div", { class: "unread" });
  const startedOver = h("p", { class: "empty" });
  const lanes = h("div", { class: "lanes panel" });
  const talk = h("div", { class: "talk" });
  const filter = h("div", { class: "filter" }, kinds, search, counted, all);
  const inspector = h("div", { class: "inspector panel" });
  const callList = h("details", {});
  /** @type {RowList<Item>} */
  const list = new RowList(
    (item) => recordRow(item, state.open, state.data?.times?.started_unix),
    (item) => {
      if ("fold" in item) {
        const first = item.fold[0].byte;
        if (!state.open.delete(first)) state.open.add(first);
        return redraw();
      }
      select(item);
    },
  );
  const split = h("div", { class: "split" });
  // The lanes come in whole lanes, so their grip steps by one.
  const lanePx = parseFloat(getComputedStyle(document.documentElement).getPropertyValue("--lane"));
  const grips = {
    side: grip({ name: "run.side", across: "x", sized: inspector, on: split, property: "--side", title: "Drag to widen the right column" }),
    lanes: grip({ name: "run.lanes", across: "y", sized: lanes, on: lanes, property: "--pane", stepPx: lanePx, title: "Drag to size the lanes" }),
    talk: grip({ name: "run.talk", across: "y", sized: talk, on: talk, property: "--pane", title: "Drag to size the diagram" }),
  };
  // The diagram of who talks to whom, on a panel of its own above the inspector.
  const diagram = h("section", { class: "roles panel" }, talk, grips.talk);
  fill(split, h("div", { class: "left" }, lanes, grips.lanes, h("section", { class: "log panel" }, filter, list.node)), grips.side, h("div", { class: "right" }, diagram, result, inspector));
  show(h("section", { class: "head panel" }, h("div", { class: "headline" }, verdict, traces, again, cancelBox), facts, failed, unread), startedOver, split);

  // Selects `record`: the lanes and the diagram light what it concerns.
  const select = (/** @type {LogRecord} */ record) => {
    setQuery({ sel: record.byte });
    redraw();
    list.select(list.selected, true);
    inspectRecord(record, inspector, recordLinks(record), state.data, failureOf);
  };
  // Selects the record that states `failure`, which the lanes and the diagram then light; a
  // record not read yet, or not among those shown, is selected by its byte alone.
  const pickFailure = (/** @type {Failure} */ failure) => {
    const record = state.shown.find((r) => r.byte === failure.mark.record);
    if (record) return select(record);
    setQuery({ sel: failure.mark.record });
    redraw();
  };
  // A failure as the inspector shows it.
  const failureOf = (/** @type {Failure} */ failure) => {
    const data = /** @type {Run} */ (state.data);
    return failureBox(data.id, failure, { base, started: data.times?.started_unix, pick: pickFailure });
  };
  // The record before (`step` -1) or after one on the lane `name`, among those shown.
  const near = (/** @type {LogRecord} */ record, /** @type {string} */ name, /** @type {number} */ step) => {
    const at = state.shown.indexOf(record);
    for (let index = at + step; at >= 0 && index >= 0 && index < state.shown.length; index += step) {
      if (lanesOf(state.shown[index]).has(name)) return state.shown[index];
    }
    return null;
  };
  // The buttons of a record's inspector: to its lanes, to their roles, and to the records before
  // and after it on each lane.
  const recordLinks = (/** @type {LogRecord} */ record) => {
    const data = /** @type {Run} */ (state.data);
    const names = [...lanesOf(record)];
    const roles = new Set();
    for (const lane of data.lanes || []) {
      const role = laneRole(lane, data);
      if (role && names.includes(lane.lane)) roles.add(role);
    }
    const keep = (/** @type {string} */ title, /** @type {Record<string, string | null>} */ change, /** @type {string} */ text) =>
      h("button", { title, onclick: () => (setQuery(change), redraw()) }, text);
    const step = (/** @type {string} */ name, /** @type {number} */ by) => {
      const other = near(record, name, by);
      const text = by < 0 ? "← Previous on " + name : "Next on " + name + " →";
      return other && h("button", { title: "Select the record " + (by < 0 ? "before" : "after") + " this one on " + name, onclick: () => select(other) }, text);
    };
    return [
      ...names.map((name) => keep("Keep the records of " + name, { lane: name, role: null }, name)),
      ...[...roles].map((role) => keep("Keep the records of role " + role, { role, lane: null }, "Role " + role)),
      ...names.flatMap((name) => [step(name, -1), step(name, 1)]),
    ];
  };
  // Selects the record of lane `name` nearest `time`.
  const nearest = (/** @type {string} */ name, /** @type {number} */ time) => {
    const of = state.shown.filter((record) => lanesOf(record).has(name));
    const distance = (/** @type {LogRecord} */ record) => Math.abs((record.when ?? 0) - time);
    if (of.length) select(of.reduce((best, record) => (distance(record) < distance(best) ? record : best)));
  };
  // The inspector with nothing selected: the run's summary.
  const unselected = () => (state.data && state.data.summary ? h("pre", { class: "summary" }, state.data.summary) : null);

  const pickLane = (/** @type {Lane} */ lane) => {
    const off = route().query.get("lane") === lane.lane;
    setQuery({ lane: off ? null : lane.lane, role: null, sel: null });
    redraw();
    if (off) fill(inspector, unselected());
    else inspectLane(lane, inspector, nearest, /** @type {Run} */ (state.data), callList, failureOf);
  };

  // Draws what the address's filters keep of the records read, and what the selection lights.
  const redraw = () => {
    const data = state.data;
    if (!data) return;
    const { query } = route();
    // A stretch of time is kept only whole, from both its ends.
    const stretch = query.has("from") && query.has("to");
    const from = stretch ? Number(query.get("from")) : null;
    const to = stretch ? Number(query.get("to")) : null;
    const lane = query.get("lane");
    const role = query.get("role");
    /** @type {Map<string, Lane[]>} the calls on each resident sandbox */
    const jobs = new Map();
    for (const job of data.lanes || []) {
      if (!job.service) continue;
      const here = jobs.get(job.service);
      if (here) here.push(job);
      else jobs.set(job.service, [job]);
    }
    const laneJobs = (lane && jobs.get(lane)) || [];
    const laneNames = lane && new Set([lane, ...laneJobs.map((job) => job.lane)]);
    callList.hidden = !laneJobs.length;
    if (laneJobs.length) drawOnce(callList, [lane, laneJobs.length], () => {
      const items = laneJobs.map((job) => {
        const label = clock(job.sandbox.asked) + " " + job.lane + (job.holder ? " of " + job.holder : "");
        return h("li", {}, h("button", { class: "mono", onclick: () => pickLane(job) }, label));
      });
      fill(callList, h("summary", {}, plural(laneJobs.length, "call")), h("ul", {}, items));
    });
    const roleLanes = role && data.lanes ? new Set(data.lanes.filter((l) => laneRole(l, data) === role).map((l) => l.lane)) : null;
    const kind = query.get("kind");
    const needle = search.value.trim().toLowerCase();
    const shown = state.records.filter((record) => {
      if (!holds(record, kind, needle)) return false;
      const at = record.when ?? 0;
      if (from !== null && to !== null && (at < from || at > to)) return false;
      const of = lanesOf(record);
      if (laneNames && ![...of].some((name) => laneNames.has(name))) return false;
      if (roleLanes && ![...of].some((name) => roleLanes.has(name))) return false;
      return true;
    });
    const parts = [];
    if (from !== null) parts.push("between " + clock(from) + " and " + clock(to));
    if (lane) parts.push("of " + lane);
    if (role) parts.push("of role " + role);
    if (kind) parts.push("of kind " + kind);
    if (needle) parts.push("holding “" + needle + "”");
    const reading = state.caught ? "" : ", reading the log: " + bytes(state.cursor) + " of " + bytes(data.end);
    counted.textContent = shown.length.toLocaleString("en-US") + " of " + plural(state.records.length, "record") + (parts.length ? ", " + parts.join(", ") : "") + reading;
    fill(all, parts.length ? h("button", { title: "Remove every filter", onclick: () => ((search.value = ""), setQuery({ from: null, to: null, lane: null, role: null, kind: null, q: null }), redraw()) }, "Show all") : null);
    drawKinds(kinds, state, kind);
    const selected = query.get("sel");
    const holder = shown.find((record) => String(record.byte) === selected);
    if (holder) {
      for (const item of folded(shown, new Set())) if ("fold" in item && item.fold.includes(holder)) state.open.add(item.fold[0].byte);
    }
    state.shown = shown;
    const lit = litOf(data, { lane, role, needle }, holder, shown);
    // Before the list, whose pane a lane added shrinks, so that a list kept to its end lands there.
    drawLanes(lanes, data, jobs, { from, to, lit }, lane, (change) => (setQuery(change), redraw()), pickLane, pickFailure);
    // The failure that the diagram marks: the one selected, by its record or its lane, else the
    // one the run ended with.
    const picked = data.failures.find((failure) => String(failure.mark.record) === selected) ?? (data.lanes || []).find((l) => l.lane === lane)?.failure ?? data.failure;
    const items = folded(shown, state.open);
    fitColumns();
    list.set(items, holder ? items.indexOf(holder) : -1);
    drawTalk(talk, data, lit, role, picked, (name) => {
      const off = name === null || name === role;
      setQuery({ role: off ? null : name, lane: null, sel: null });
      redraw();
      if (off) fill(inspector, unselected());
      else inspectRole(name, inspector, data);
    });
    if (selected !== state.selected) lanes.querySelector(".lane.lit")?.scrollIntoView({ block: "nearest" });
    state.selected = selected;
  };

  // The log's columns of who and of kind are as wide as the longest of each in the whole log, within
  // bounds, so that no kind is cut short while the text beside it has room. Each record is
  // measured once, as the log is read.
  const fit = { seen: 0, who: WHO_MIN_CHARS, kind: KIND_MIN_CHARS };
  const fitColumns = () => {
    if (state.records.length < fit.seen) Object.assign(fit, { seen: 0, who: WHO_MIN_CHARS, kind: KIND_MIN_CHARS });
    for (; fit.seen < state.records.length; fit.seen++) {
      const record = state.records[fit.seen];
      fit.kind = Math.max(fit.kind, saidOf(record)[0].length);
      fit.who = Math.max(fit.who, [...lanesOf(record)].join(" ").length);
    }
    list.node.style.setProperty("--who-chars", String(Math.min(WHO_MAX_CHARS, fit.who)));
    list.node.style.setProperty("--kind-chars", String(Math.min(KIND_MAX_CHARS, fit.kind)));
  };

  // Draws the run's verdict and facts, which change with its clock.
  const drawHeading = () => {
    const data = /** @type {Run} */ (state.data);
    const failure = data.failure;
    // How the run ended is its mark's colour; the words are in the mark's card.
    const [how, more] = verdictOf(data);
    const mark = card(h("span", { class: "state", tabindex: 0 }, h("span", { class: "mark " + tone(data) })), () =>
      cardRows([
        ["State", how],
        ["", more],
        ["Run", data.id],
        ["Made by", data.other_build ? "pumpkin " + data.other_build : null],
      ]),
    );
    fill(verdict, mark, h("span", { class: "task" }, data.task || data.id));
    drawOnce(result, data.result, () => drawResult(result, data.result));
    drawOnce(failed, failure, () => fill(failed, failure ? failureParts(data.id, failure, { base, started: data.times?.started_unix, pick: pickFailure }) : null));
    drawFacts(facts, data);
  };
  // Draws the run: its heading, the command that runs it again, its cancel.
  const drawRun = () => {
    const data = /** @type {Run} */ (state.data);
    drawHeading();
    drawAgain(again, data);
    drawTraces(traces, data);
    drawCancel();
    fill(unread, data.unread ? h("p", { class: "warn" }, h("span", { class: "mark warn" }), plural(data.unread.count, "line") + " of the log can't be read: " + data.unread.said) : null);
    drawFollow();
  };
  const drawFollow = () => {
    const data = state.data;
    const live = !!data && data.live;
    fill(
      byId("follow"),
      live ? h("span", { class: "mark live", title: "The run is live: its log is read every " + POLL_MS / 1000 + " s" }) : null,
      live ? (list.tail ? "Following the end" : h("button", { title: "Go to the end of the log, and keep to it (f)", onclick: () => list.end() }, "Follow the end")) : null,
    );
  };
  list.node.addEventListener("scroll", drawFollow);

  kinds.addEventListener("change", () => (setQuery({ kind: kinds.value || null }), redraw()));
  search.addEventListener("input", () => (setQuery({ q: search.value.trim().toLowerCase() || null }), redraw()));
  search.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      search.blur();
    }
  });
  setKeys((event) => {
    if (typing()) return;
    const step = { j: 1, k: -1 }[event.key];
    if (event.key === "/") {
      event.preventDefault();
      search.focus();
      search.select();
    } else if (step) {
      // The next record shown, past folds closed.
      event.preventDefault();
      const items = list.items;
      for (let index = list.selected + step; index >= 0 && index < items.length; index += step) {
        const item = items[index];
        if (!("fold" in item)) return select(item);
      }
    } else if (event.key === "f" && state.data && state.data.live) {
      list.end();
      drawFollow();
    } else if (event.key === "Escape") {
      setQuery({ sel: null, lane: null, role: null, from: null, to: null });
      fill(inspector, unselected());
      redraw();
    }
  });

  // The page's cancel of this run: asked for, confirmed, going (read from the server until
  // `pumpkin cancel` has ended), done or refused, with the server's own words as text.
  /** @type {{ phase: "idle" | "confirm" | "going" | "done" | "refused", said: string, error: string, exit: number | null }} */
  const cancel = { phase: "idle", said: "", error: "", exit: null };
  const watchCancel = async () => {
    while (current()) {
      try {
        const reply = await get(base + "/cancelling");
        cancel.said = reply.said;
        if (reply.ended) {
          [cancel.phase, cancel.exit] = ["done", reply.exit];
          return drawCancel();
        }
      } catch (error) {
        [cancel.phase, cancel.error] = ["refused", error instanceof Error ? error.message : String(error)];
        return drawCancel();
      }
      drawCancel();
      await new Promise((resolve) => setTimeout(resolve, CANCEL_POLL_MS));
    }
  };
  const startCancel = async () => {
    [cancel.phase, cancel.said, cancel.error] = ["going", "", ""];
    drawCancel();
    try {
      const { status, body } = await post(base + "/cancel");
      if (status === 202) return watchCancel();
      // A server without a cancel answers that nothing is there, or that it reads by GET alone.
      cancel.error = status === 404 || status === 405 ? "This pumpkin view doesn't serve a cancel: run `pumpkin cancel " + id + "` in a terminal instead." : body.error || "Status " + status;
      // A cancel of the page's own that is under way is watched, as one just started is.
      if (status === 409) {
        drawCancel();
        return watchCancel();
      }
    } catch (error) {
      cancel.error = "The page's server could not be reached: " + (error instanceof Error ? error.message : String(error));
    }
    cancel.phase = "refused";
    drawCancel();
  };
  const drawCancel = () => {
    const live = !!state.data && state.data.live;
    const said = cancel.said ? h("pre", {}, cancel.said) : null;
    const to = (/** @type {typeof cancel.phase} */ phase) => () => ((cancel.phase = phase), drawCancel());
    const ask = h("button", { class: "danger", title: "Stop this run: asks you to confirm first", onclick: to("confirm") }, "Cancel run");
    if (cancel.phase === "idle") return fill(cancelBox, live ? ask : null);
    if (cancel.phase === "confirm") {
      return fill(
        cancelBox,
        h(
          "div",
          { class: "sure" },
          h("p", {}, "Cancel run ", h("code", {}, id), "? It stops the run, waits until it has ended, removes what it left, and writes its outcome.json. This ends a real run and can't be undone."),
          h("button", { class: "danger", title: "Send the cancel now", onclick: startCancel }, "Yes, cancel"),
          h("button", { title: "Leave the run as it is", onclick: to("idle") }, "Keep it running"),
        ),
      );
    }
    if (cancel.phase === "going") return fill(cancelBox, h("div", { class: "sure" }, h("p", {}, "Cancelling run ", h("code", {}, id), "…"), cancel.error ? h("p", { class: "error" }, cancel.error) : null, said));
    const how = cancel.exit === 0 ? "The run has ended and what it left is removed." : cancel.exit === 3 ? "The run has ended, but its provider was not reached." : "The cancel exited with status " + cancel.exit + ".";
    if (cancel.phase === "done") return fill(cancelBox, h("div", { class: "sure" }, h("p", {}, how), said, h("button", { title: "Close this", onclick: to("idle") }, "OK")));
    return fill(
      cancelBox,
      h("div", { class: "sure" }, h("p", { class: "error" }, cancel.error), live ? h("button", { class: "danger", title: "Ask to cancel again", onclick: to("confirm") }, "Cancel run") : null, h("button", { title: "Close this", onclick: to("idle") }, "OK")),
    );
  };

  const take = (/** @type {LogRecord[]} */ records, /** @type {number} */ next) => {
    state.records.push(...records);
    state.cursor = next;
  };
  // Forgets what was read, to read the log again from its start.
  const again0 = () => {
    Object.assign(state, { data: null, records: [], cursor: 0, caught: false, drawnAt: 0 });
    state.open.clear();
    delete callList.dataset.key;
  };
  // The first time the log is read to the run's end: a live run goes to its end, and what the
  // address selects is inspected.
  const caughtUp = () => {
    const data = /** @type {Run} */ (state.data);
    state.caught = true;
    if (data.live && !route().query.has("sel")) list.tail = true;
    redraw();
    if (state.inspected) return;
    state.inspected = true;
    const { query } = route();
    const lane = query.get("lane") && (data.lanes || []).find((l) => l.lane === query.get("lane"));
    const record = state.records.find((r) => String(r.byte) === query.get("sel"));
    const role = query.get("role");
    if (record) {
      inspectRecord(record, inspector, recordLinks(record), data, failureOf);
      list.select(list.selected, true);
    } else if (lane) inspectLane(lane, inspector, nearest, data, callList, failureOf);
    else if (role) inspectRole(role, inspector, data);
    else fill(inspector, unselected());
  };
  const read = async (/** @type {() => boolean} */ current) => {
    if (!state.data) {
      /** @type {Run} */
      const data = await get(base);
      if (!current()) return null;
      state.data = data;
      drawRun();
      redraw();
      return 0;
    }
    const data = state.data;
    if (state.cursor < data.end) {
      /** @type {Window} */
      const window = await get(base + "/log?at=" + state.cursor + "&log_epoch=" + data.log_epoch);
      if (!current()) return null;
      if (window.end < state.cursor) return again0(), 0;
      state.records.push(...pastWindow(window, state));
      if (state.cursor >= data.end) caughtUp();
      else if (state.records.length - state.drawnAt >= DRAW_EVERY_RECORDS) {
        state.drawnAt = state.records.length;
        redraw();
      } else counted.textContent = plural(state.records.length, "record") + ", reading the log: " + bytes(state.cursor) + " of " + bytes(data.end);
      return 0;
    }
    if (!state.caught) caughtUp();
    if (!data.live) return null;
    /** @type {Since} */
    const since = await get(base + "/since?log=" + state.cursor + "&log_epoch=" + data.log_epoch);
    if (!current()) return null;
    if (since.end < state.cursor) return again0(), 0;
    if (!since.run) {
      // It ended with no record after the last: its ending is in the run alone.
      state.data = await get(base);
      if (!current()) return null;
      drawRun();
      redraw();
      return null;
    }
    state.data = since.run;
    if (!since.records) {
      // The heading alone is drawn: drawn again, the log's rows, the lanes and the buttons would
      // lose a reader's selection, focus, hover or press in them.
      drawHeading();
      return POLL_MS;
    }
    take(since.records, since.next ?? since.end);
    drawRun();
    redraw();
    if (state.cursor < since.end) return 0;
    return since.live ? POLL_MS : null;
  };
  const current = follow(async (current) => {
    try {
      return await read(current);
    } catch (error) {
      // The log started over (409), or a byte starts no line of it (400): what was read isn't this
      // log.
      if (error instanceof Refused && (error.status === 409 || error.status === 400) && state.data) {
        again0();
        startedOver.textContent = "The log started over: read again from its start.";
        return 0;
      }
      throw error;
    }
  });
}

// What a selection lights, or null where everything is lit: the lanes, and the roles of the
// diagram, that the selection concerns. A record selected (`holder`) wins over a role, a role
// over a search, a search over a lane. `here` is the record's time, `focus` a role picked, whose
// neighbours stay lit.
/** @typedef {{ lanes: Set<string>, roles: Set<string>, focus: string | null, here: number | null }} Lit */
/** @param {Run} data @param {{ lane: string | null, role: string | null, needle: string }} view @param {LogRecord | undefined} holder @param {LogRecord[]} shown @returns {Lit | null} */
function litOf(data, view, holder, shown) {
  const known = data.lanes || [];
  // The lit set of the lanes among `names`, with their roles; null if none is.
  /** @param {Set<string>} names @returns {Lit | null} */
  const of = (names) => {
    const lanes = new Set();
    /** @type {Set<string>} */
    const roles = new Set();
    for (const lane of known) {
      if (!names.has(lane.lane)) continue;
      lanes.add(lane.lane);
      if (lane.service) lanes.add(lane.service);
      const role = laneRole(lane, data);
      if (role) roles.add(role);
    }
    return lanes.size ? { lanes, roles, focus: null, here: null } : null;
  };
  if (holder) {
    const lit = of(lanesOf(holder));
    if (lit) {
      lit.here = holder.when;
      if (holder.event?.kind === "model_answered") lit.roles.add("model");
    }
    return lit;
  }
  if (view.role) {
    const lanes = new Set(known.filter((lane) => laneRole(lane, data) === view.role).map((lane) => lane.lane));
    return { lanes, roles: new Set([view.role]), focus: view.role, here: null };
  }
  if (view.needle) return of(new Set(shown.flatMap((record) => [...lanesOf(record)])));
  if (view.lane) return of(new Set([view.lane]));
  return null;
}

// The run's facts in a line, each a muted name and its value: its task, when it started, how
// long it ran of its hard end, what its model requests took, and what it cost.
/** @param {HTMLElement} facts @param {Run} data */
// Draws `node` with `draw` only when `key` differs from what it was last drawn for, so that what
// a reader opened in it stays open while the heading is drawn again with the run's clock.
/** @param {HTMLElement} node @param {any} key @param {() => void} draw */
function drawOnce(node, key, draw) {
  const text = JSON.stringify(key ?? null);
  if (node.dataset.key === text) return;
  node.dataset.key = text;
  draw();
}

// How long a run ran, with its units, as the list of runs and the run's state write it: `42s`,
// `8m24s`, `1h08m`. A
// time on the axis or in the log is a place since the run's start, and keeps the clock's form.
/** @param {number | null | undefined} seconds */
function lastedFor(seconds) {
  if (seconds === null || seconds === undefined) return "–";
  const whole = Math.floor(seconds);
  const [hours, minutes, secs] = [Math.floor(whole / 3600), Math.floor((whole % 3600) / 60), whole % 60];
  if (hours) return hours + "h" + String(minutes).padStart(2, "0") + "m";
  return minutes ? minutes + "m" + String(secs).padStart(2, "0") + "s" : secs + "s";
}

function drawFacts(facts, data) {
  const times = data.times;
  const model = data.model;
  // A number of the run: the figure, what qualifies it in small beside it, its name below, and a
  // card of what matters less about it.
  /** @param {string} name @param {string} value @param {Array<[string, import("./h.js").Child] | null | false>} rows @param {string} [small] @param {string} [tone] */
  const fact = (name, value, rows, small = "", tone = "") =>
    card(h("div", { class: "tile fact", tabindex: 0 }, h("div", { class: "v" }, value, small ? h("small", { class: tone }, small) : null), h("div", { class: "l" }, name)), () => cardRows(rows));
  const ran = times && (times.end ?? times.now ?? times.last);
  const stage = data.live ? data.stage : null;
  const failed = model ? model.failed.reduce((sum, f) => sum + f.count, 0) : 0;
  // How long each request in flight has gone unanswered, to now, or to the end of a run that ended.
  const at = times ? (times.now ?? times.end ?? times.last) : 0;
  const flying = model ? [...model.in_flight].sort((a, b) => a.since - b.since) : [];
  const notOk = "not forwarded whole with a 2xx status: cut short, refused, barred or abandoned by the route, or answered with another status";
  const number = (/** @type {number | null | undefined} */ n) => count(n, "").trimEnd();
  const dropped = model && model.dropped_headers.length ? model.dropped_headers.join(", ") + (model.more_dropped_headers ? ", and more" : "") : null;
  fill(
    facts,
    // The first figure is the time that matters now: of a run that goes on, how long its stage has
    // lasted, of how long it may, under the stage's name, as `ls` shows them; of a run that ended,
    // how long it ran. The card has the rest.
    fact(
      stage ? sentence(stage.name) : "Ran",
      stage ? stage.time : lastedFor(ran),
      [
        ["Run", stage && times ? lastedFor(ran) + " so far" : null],
        ["Hard end", clock(times && times.hard_end)],
        ["Used", times && times.hard_end ? Math.round((100 * (ran ?? 0)) / times.hard_end) + "% of the hard end" : null],
      ],
      stage ? stage.bound : "",
    ),
    times ? fact("Started", utc(times.started_unix, 0).slice(5, 16), [["Started", utc(times.started_unix, 0).slice(0, 19) + " UTC"]], "UTC") : null,
    model
      ? fact(
          "Requests",
          number(model.requests),
          [
            ["Failed", model.failed.map((f) => f.count + " " + f.code).join(", ")],
            ["Not 2xx", model.not_2xx ? plural(model.not_2xx, "answer") + " " + notOk : null],
            ["Median took", model.took.median !== null ? span(model.took.median) + ", of " + plural(model.took.count, "answer") : null],
            ["First token", model.first_token.median !== null ? span(model.first_token.median) + ", the median of " + plural(model.first_token.count, "answer") : null],
            ["In flight", flying.map((f) => "Request " + f.request + " for " + span(at - f.since)).join(", ")],
            ["Headers the route dropped", dropped],
          ],
          [failed ? failed + " failed" : "", model.not_2xx ? model.not_2xx.toLocaleString("en-US") + " not 2xx" : ""].filter(Boolean).join(", "),
          failed ? "error" : "",
        )
      : null,
    model
      ? fact("Tokens in · out", number(model.tokens.input) + " · " + number(model.tokens.output), [
          ["Input", model.tokens.input === null ? "Unknown: no response reported its tokens" : count(model.tokens.input, "tok")],
          ["Cached", model.tokens.cached !== null ? count(model.tokens.cached, "tok") + " of the input" : null],
          ["Output", model.tokens.output === null ? null : count(model.tokens.output, "tok")],
        ])
      : null,
    data.cost ? fact("Cost", data.cost.shown, [["Cost", data.cost.said]]) : null,
    data.driver
      ? fact(
          "Driver's CPU",
          data.driver.shown,
          data.driver.rows.map(([name, value]) => [sentence(name), value]),
        )
      : null,
  );
}

// A run's verdict in two: a few words, and the rest of the sentence. A run with a result says
// that; a run that failed, its failure's state, then its verdict; any other, its verdict less the
// run's id, which the page's address shows, up to its first colon.
/** @param {Run} data @returns {[string, string]} */
function verdictOf(data) {
  if (data.result) return ["Finished with a result", ""];
  if (data.failure) return [sentence(data.failure.state), data.verdict];
  const lead = "Run " + data.id + " ";
  const ended = data.verdict.startsWith(lead);
  const text = ended ? data.verdict.slice(lead.length).replace(/^(was|is) /, "") : data.verdict;
  const at = text.indexOf(": ");
  if (at < 0) return [sentence(text), ""];
  return [sentence(text.slice(0, at)), ended ? sentence(text.slice(at + 2)) + "." : text.slice(at + 2)];
}

// The command that runs the run again from here, with a copy button, or why there is none; nothing
// if the reply has no `again`.
/** @param {HTMLElement} node @param {Run} data */
function drawAgain(node, data) {
  node.hidden = data.again === undefined;
  if (data.again === undefined) return;
  const command = data.again.command;
  if (command === undefined) return fill(node, card(h("button", { disabled: true }, "Copy command"), () => data.again?.error ?? "No command runs this run again."));
  fill(
    node,
    card(h("button", { onclick: () => navigator.clipboard && navigator.clipboard.writeText(command) }, "Copy command"), () => [h("p", { class: "why" }, "The command that runs this run again"), h("code", {}, command)]),
  );
}

// A link to the trace of each caller with model answers: the agent, each call.
/** @param {HTMLElement} node @param {Run} data */
function drawTraces(node, data) {
  const callers = (data.lanes || []).filter((lane) => (lane.kind === "agent" || lane.kind === "target") && lane.answers && lane.answers.count > 0);
  fill(
    node,
    // Each says what it opens, and looks like what it is: a link to that caller's trace.
    callers.map((lane) =>
      h(
        "a",
        { href: "#/run/" + encodeURIComponent(data.id) + "/trace?caller=" + encodeURIComponent(lane.caller ?? ""), class: "kind " + lane.kind, title: "What " + lane.lane + " and the model said to each other: " + plural(lane.answers?.count ?? 0, "answer") },
        h("span", { class: "mark" }),
        "Trace of " + (lane.kind === "agent" && lane.role ? lane.role : lane.lane),
        h("span", { class: "go" }, "→"),
      ),
    ),
  );
}

// Shows role `name` in the inspector: its kind, its place, and its model route, each entry's API,
// upstream and models, by the name the upstream takes each as; the model upstream, each role's
// route that reaches it.
/** @param {string} name @param {HTMLElement} inspector @param {Run} data */
function inspectRole(name, inspector, data) {
  const roles = data.talk ? data.talk.roles : [];
  const unknown = "Unknown: the build of pumpkin that made this record kept no table of its routes";
  /** @param {Role} role */
  const routes = (role) => {
    if (role.routes === "unknown") return h("dd", { title: unknown }, "Unknown");
    if (!role.routes.length) return h("dd", {}, "None: the role has no model route");
    return h(
      "dd",
      {},
      role.routes.map((route) =>
        h(
          "div",
          { class: "route" },
          h("div", {}, h("span", { class: "name" }, "API "), route.api, h("span", { class: "name" }, " Upstream "), route.upstream),
          h("ul", { title: "Each model the route allows, → the name the upstream takes it as" }, Object.entries(route.models).map(([model, upstream]) => h("li", {}, model + " → " + upstream))),
        ),
      ),
    );
  };
  if (name === "model") {
    const routed = roles.filter((role) => role.routes === "unknown" || role.routes.length);
    return fill(
      inspector,
      h("h3", {}, h("span", { class: "kind model" }, "model"), " the route's upstream"),
      h("p", { class: "said" }, "Each role with a route reaches it, as its route says:"),
      routed.length ? h("dl", {}, routed.flatMap((role) => [h("dt", {}, role.name), routes(role)])) : h("p", { class: "empty" }, "No role has a model route."),
    );
  }
  const role = roles.find((r) => r.name === name);
  if (!role) return fill(inspector, h("p", { class: "empty" }, "The record names no role " + name + "."));
  fill(
    inspector,
    h("h3", {}, h("span", { class: "kind " + (role.kind || "") }, role.kind || "role"), " ", role.name),
    h(
      "dl",
      {},
      h("dt", {}, "Place"),
      h("dd", {}, role.place || "-"),
      role.lifetime ? [h("dt", {}, "Sandbox lifetime"), h("dd", {}, role.lifetime)] : null,
      role.final_target ? [h("dt", {}, "Final call"), h("dd", {}, "The final call's target, in which the body whose return is the run's result runs")] : null,
      h("dt", {}, "Model route"),
      routes(role),
    ),
  );
}

// Shows `record` in the inspector: its kind and time, the failure it states, if it states one,
// its fields that are no object, the buttons `links`, a link to the payload it names, if any,
// then the record whole, as the log holds it, its strings cut, coloured.
/** @param {LogRecord} record @param {HTMLElement} inspector @param {Array<HTMLElement | false | null | undefined>} links @param {Run | null} data @param {(failure: Failure) => HTMLElement} failureOf */
function inspectRecord(record, inspector, links, data, failureOf) {
  const [kind] = saidOf(record);
  const started = data && data.times && data.times.started_unix;
  const { byte, when, ...kept } = record;
  const event = /** @type {Record<string, any>} */ (record.event || {});
  const payload = data && payloadOf(record);
  const failure = data && data.failures.find((failure) => failure.mark.record === byte);
  /** @type {Array<[string, any]>} */
  const scalars = [
    ["seq", record.seq],
    ["at", clock(when)],
    ["UTC", utc(started, when)],
    ["byte", byte.toLocaleString("en-US")],
    ...Object.entries(event).filter(([name, value]) => name !== "kind" && (value === null || typeof value !== "object")),
  ];
  fill(
    inspector,
    h("h3", {}, h("span", { class: "kind" }, kind), " ", h("span", { class: "t", title: utc(started, when) }, clock(when))),
    failure ? failureOf(failure) : null,
    h("dl", {}, scalars.filter(([, value]) => value !== undefined && value !== "").flatMap(([name, value]) => [h("dt", {}, name), h("dd", {}, String(value))])),
    links.some(Boolean) ? h("p", { class: "links" }, links) : null,
    payload && data ? payloadLink(data.id, payload) : null,
    jsonText(kept),
  );
}
