// A run: its verdict and the command that runs it again, its sandboxes' lanes, who talks to whom,
// its log as a list that draws only the rows in view, and the inspector of what is selected.
//
// The log is read from its start in windows (`/log?at=<byte>`) up to the byte that the run's
// reply was read to, then, while the run is live, polled from where the last read ended
// (`/since?log=<byte>`): a reply with the run replaces it, and appends the records it holds; one
// of only `end` and `live` is of a run that ended with no record past those read. A refusal of
// the byte, or a log shorter than it, means the log started over, and the run is read again from
// its start. In the static mode, where no file answers `/since`, the run is read again at each
// poll, and the log's last window is read again from the byte it starts at, its records past the
// last read taken.

import {
  POLL_MS,
  Refused,
  STATIC,
  bytes,
  byId,
  clock,
  count,
  fill,
  follow,
  get,
  grip,
  h,
  plural,
  post,
  route,
  setKeys,
  setQuery,
  show,
  span,
  svg,
  tone,
  typing,
  utc,
} from "./h.js";
import { drawKinds, folded, holds, lanesOf, pastWindow, recordRow, saidOf, windowAt } from "./records.js";
import { RowList } from "./rows.js";
import { jsonText, payloadLink, payloadOf } from "./payload.js";
import { drawTalk, laneRole } from "./talk.js";

/**
 * @typedef {object} Run `/api/runs/<id>`
 * @property {string} id
 * @property {string | null} task
 * @property {boolean} live
 * @property {string} state
 * @property {boolean} has_result
 * @property {string} verdict
 * @property {{ command: string, error?: undefined } | { error: string, command?: undefined } | undefined} again
 * @property {string | null} other_build
 * @property {Times | null} times
 * @property {Lane[] | null} lanes
 * @property {Talk | null} talk
 * @property {ModelUse | null} model
 * @property {Cost | null} cost null if nothing of it has a price
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
 * @typedef {object} Lane a sandbox's lane: the agent's, a call's (`job <n>`) or a lease's
 * @property {string} lane
 * @property {string} [caller] the caller that the lane's trace is of, `agent` or `job-<n>`; none for a lease
 * @property {"agent" | "target" | "runner"} kind
 * @property {string | null} [role]
 * @property {{ function: string } | "final"} [of] whose body a call's lane calls: a job function's, by its name, or the final call's
 * @property {string | null} [grant] a lease's session
 * @property {string} [holder]
 * @property {{ place: string, asked: number, started: number | null, gone: number | null, deleted: boolean | null }} sandbox
 * @property {string} said
 * @property {number[]} [answered] the times of its model answers, at most 500 of them, to draw
 * @property {number[]} [failed] those of `answered` that stand for a failed answer
 * @property {{ count: number, failed: number }} [answers] how many model answers it has, and failed
 * @property {Work[] | null} [work]
 * @typedef {{ kind: string, label: string, from: number, to: number | null, end: any }} Work
 * @typedef {{ roles: Role[], grants?: Grant[], edges: Edge[] }} Talk
 * @typedef {{ from: string, to: string, kind: string }} Grant a grant of the IR: its holder and the role it reaches
 * @typedef {{ name: string, kind: string | null, place: string | null, routed: boolean, final_target: boolean, routes: Route[] | "unknown" }} Role
 * @typedef {{ api: string, upstream: string, models: Record<string, string> }} Route an entry of a role's model route: each model by the name the upstream takes it as
 * @typedef {{ from: string, to: string, kind: string, count: number, bytes: number | null, back_bytes: number | null }} Edge
 * @typedef {import("./records.js").LogRecord} LogRecord
 * @typedef {import("./records.js").Window} Window
 * @typedef {{ end: number, live: boolean, run?: Run, records?: LogRecord[], next?: number }} Since `/since`
 * @typedef {import("./records.js").Item} Item
 */

// The bounds of the characters of the lanes' names and places before the tracks, which the style
// turns into `--label` in its monospace.
const LABEL_MIN_CHARS = 18;
const LABEL_MAX_CHARS = 48;
// How many records the log's list is drawn again after while the log is read from its start.
const DRAW_EVERY_RECORDS = 2000;
// How often a cancel under way is read.
const CANCEL_POLL_MS = 1000;

/** What a call's lane calls, as words. @param {{ function: string } | "final"} of */
function calls(of) {
  return of === "final" ? "the final call" : "job function " + of.function;
}

// ---- the run's page

/** @param {string} id */
export async function runView(id) {
  const base = "/api/runs/" + encodeURIComponent(id);
  // `cursor` and `at` are where the log is read (`Reading`); `open` holds the first record's byte
  // of each fold opened; `shown` the records the address's filters keep; `selected` the `sel`
  // last drawn.
  /** @type {{ data: Run | null, records: LogRecord[], cursor: number, at: number, caught: boolean, drawnAt: number, open: Set<number>, kinds: string, shown: LogRecord[], selected: string | null | undefined, inspected: boolean }} */
  const state = { data: null, records: [], cursor: 0, at: 0, caught: false, drawnAt: 0, open: new Set(), kinds: "", shown: [], selected: undefined, inspected: false };
  const { query } = route();
  const counted = h("span", { class: "counted" });
  const all = h("span", {});
  const kinds = /** @type {HTMLSelectElement} */ (h("select", { title: "Show the records of one kind" }));
  const search = /** @type {HTMLInputElement} */ (h("input", { type: "search", class: "search", placeholder: "Keep the records that hold…", title: "Keep the records that hold this text (/ to type, Esc to leave the box)", value: query.get("q") || "" }));
  const verdict = h("div", { class: "verdict" });
  const facts = h("div", { class: "facts" });
  const again = h("div", { class: "again" });
  const traces = h("div", { class: "traces" });
  const cancelBox = h("div", { class: "cancel" });
  const unread = h("div", { class: "unread" });
  const startedOver = h("p", { class: "empty" });
  const lanes = h("div", { class: "lanes panel" });
  const talk = h("div", { class: "talk panel" });
  const filter = h("div", { class: "filter" }, kinds, search, counted, all);
  const inspector = h("div", { class: "inspector panel" });
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
  fill(split, h("div", { class: "left" }, lanes, grips.lanes, filter, list.node), grips.side, h("div", { class: "right" }, talk, grips.talk, inspector));
  show(verdict, facts, again, traces, cancelBox, unread, startedOver, split);

  // Selects `record`: the lanes and the diagram light what it concerns.
  const select = (/** @type {LogRecord} */ record) => {
    setQuery({ sel: record.byte, lane: null });
    redraw();
    list.select(list.selected, true);
    inspectRecord(record, inspector, recordLinks(record), state.data);
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
  // The inspector with nothing selected: how to select, and the run's summary.
  const unselected = () => [
    h("p", { class: "empty" }, "Select a record (j and k step), a lane or a role, or drag across the lanes to keep the records of a stretch of time; Esc clears them."),
    state.data && state.data.summary ? h("pre", { class: "summary" }, state.data.summary) : null,
  ];

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
    const roleLanes = role && data.lanes ? new Set(data.lanes.filter((l) => laneRole(l, data) === role).map((l) => l.lane)) : null;
    const kind = query.get("kind");
    const needle = search.value.trim().toLowerCase();
    const shown = state.records.filter((record) => {
      if (!holds(record, kind, needle)) return false;
      const at = record.when ?? 0;
      if (from !== null && to !== null && (at < from || at > to)) return false;
      const of = lanesOf(record);
      if (lane && !of.has(lane)) return false;
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
    drawLanes(lanes, data, { from, to, lit }, lane, (change) => (setQuery(change), redraw()), (l) => inspectLane(l, inspector, nearest, data));
    const items = folded(shown, state.open);
    list.set(items, holder ? items.indexOf(holder) : -1);
    drawTalk(talk, data, lit, role, (name) => {
      const off = name === null || name === role;
      setQuery({ role: off ? null : name, lane: null, sel: null });
      redraw();
      if (off) fill(inspector, unselected());
      else inspectRole(name, inspector, data);
    });
    if (selected !== state.selected) lanes.querySelector(".lane.lit")?.scrollIntoView({ block: "nearest" });
    state.selected = selected;
  };

  // Draws the run's verdict and facts, which change with its clock.
  const drawHeading = () => {
    const data = /** @type {Run} */ (state.data);
    fill(verdict, h("span", { class: "mark " + tone(data) }), h("span", {}, data.verdict));
    drawFacts(facts, data);
  };
  // Draws the run: its heading, the command that runs it again, its cancel.
  const drawRun = () => {
    const data = /** @type {Run} */ (state.data);
    drawHeading();
    if (STATIC) fill(again);
    else drawAgain(again, data);
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
    const ask = h("button", { class: "danger", title: "Stop this run: asks you to confirm first", onclick: to("confirm") }, "Cancel run…");
    if (cancel.phase === "idle") return fill(cancelBox, live && !STATIC ? ask : null);
    if (cancel.phase === "confirm") {
      return fill(
        cancelBox,
        h(
          "div",
          { class: "sure" },
          h("p", {}, "Cancel run ", h("code", {}, id), "? It stops the run, waits until it has ended, removes what it left, and writes its outcome.json. This ends a real run and can't be undone."),
          h("button", { class: "danger", title: "Send the cancel now", onclick: startCancel }, "Cancel run"),
          h("button", { title: "Leave the run as it is", onclick: to("idle") }, "Keep it running"),
        ),
      );
    }
    if (cancel.phase === "going") return fill(cancelBox, h("div", { class: "sure" }, h("p", {}, "Cancelling run ", h("code", {}, id), "…"), cancel.error ? h("p", { class: "error" }, cancel.error) : null, said));
    const how = cancel.exit === 0 ? "The run has ended and what it left is removed." : cancel.exit === 3 ? "The run has ended, but its provider was not reached." : "The cancel exited with status " + cancel.exit + ".";
    if (cancel.phase === "done") return fill(cancelBox, h("div", { class: "sure" }, h("p", {}, how), said, h("button", { title: "Close this", onclick: to("idle") }, "OK")));
    return fill(
      cancelBox,
      h("div", { class: "sure" }, h("p", { class: "error" }, cancel.error), live ? h("button", { class: "danger", title: "Ask to cancel again", onclick: to("confirm") }, "Cancel run…") : null, h("button", { title: "Close this", onclick: to("idle") }, "OK")),
    );
  };

  const take = (/** @type {LogRecord[]} */ records, /** @type {number} */ next) => {
    state.records.push(...records);
    state.cursor = next;
  };
  // Forgets what was read, to read the log again from its start.
  const again0 = () => {
    Object.assign(state, { data: null, records: [], cursor: 0, at: 0, caught: false, drawnAt: 0 });
    state.open.clear();
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
    if (lane) inspectLane(lane, inspector, nearest, data);
    else if (role && !record) inspectRole(role, inspector, data);
    else if (record) {
      inspectRecord(record, inspector, recordLinks(record), data);
      list.select(list.selected, true);
    } else fill(inspector, unselected());
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
      const at = windowAt(state);
      /** @type {Window} */
      const window = await get(base + "/log?at=" + at + "&log_epoch=" + data.log_epoch);
      if (!current()) return null;
      if (window.end < state.cursor) return again0(), 0;
      state.records.push(...pastWindow(window, state, at));
      if (state.cursor >= data.end) caughtUp();
      else if (state.records.length - state.drawnAt >= DRAW_EVERY_RECORDS) {
        state.drawnAt = state.records.length;
        redraw();
      } else counted.textContent = plural(state.records.length, "record") + ", reading the log: " + bytes(state.cursor) + " of " + bytes(data.end);
      return 0;
    }
    if (!state.caught) caughtUp();
    if (!data.live) return null;
    if (STATIC) {
      /** @type {Run} */
      const run = await get(base);
      if (!current()) return null;
      if (run.log_epoch !== data.log_epoch) return again0(), 0;
      state.data = run;
      drawRun();
      if (state.cursor < run.end) return 0;
      return run.live ? POLL_MS : (redraw(), null);
    }
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
    const lanes = new Set(known.filter((lane) => names.has(lane.lane)).map((lane) => lane.lane));
    if (!lanes.size) return null;
    /** @type {Set<string>} */
    const roles = new Set();
    for (const lane of known) {
      const role = laneRole(lane, data);
      if (role && lanes.has(lane.lane)) roles.add(role);
    }
    return { lanes, roles, focus: null, here: null };
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
function drawFacts(facts, data) {
  const times = data.times;
  const model = data.model;
  const fact = (/** @type {string} */ name, /** @type {string} */ value, /** @type {string | null} */ title = null) => h("span", { class: "fact", title }, h("span", { class: "name" }, name), h("span", { class: "value" }, value));
  const ran = times && (times.end ?? times.now ?? times.last);
  const failed = model ? model.failed.reduce((sum, f) => sum + f.count, 0) : 0;
  // How long each request in flight has gone unanswered, to now, or to the end of a run that ended.
  const at = times ? (times.now ?? times.end ?? times.last) : 0;
  const flying = model ? [...model.in_flight].sort((a, b) => a.since - b.since) : [];
  const notOk = "not forwarded whole with a 2xx status: cut short, refused, barred or abandoned by the route, or answered with another status";
  fill(
    facts,
    fact("Task", data.task || "-"),
    times ? fact("Started", utc(times.started_unix, 0).slice(0, 19) + " UTC") : null,
    fact("Ran", clock(ran), times && times.hard_end ? Math.round((100 * (ran ?? 0)) / times.hard_end) + "% of the hard end" : null),
    fact("Hard end", clock(times && times.hard_end)),
    model
      ? fact(
          "Requests",
          count(model.requests, "req") + (failed ? ", " + failed + " failed" : "") + (model.not_2xx ? ", " + model.not_2xx.toLocaleString("en-US") + " not 2xx" : ""),
          [model.failed.map((f) => f.count + " " + f.code).join(", "), model.not_2xx ? plural(model.not_2xx, "answer") + " " + notOk : ""].filter(Boolean).join("; ") || null,
        )
      : null,
    model ? fact("Tokens", count(model.tokens.input, "tok") + " in · " + count(model.tokens.output, "tok") + " out", model.tokens.input === null ? "Unknown: no response reported its tokens" : model.tokens.cached !== null ? count(model.tokens.cached, "tok") + " of the input cached" : null) : null,
    data.cost ? fact("Cost", data.cost.shown, data.cost.said) : null,
    model && model.took.median !== null ? fact("Median took", span(model.took.median), "Of " + plural(model.took.count, "answer")) : null,
    model && model.first_token.median !== null ? fact("First token", span(model.first_token.median), "Median, of " + plural(model.first_token.count, "answer")) : null,
    flying.length
      ? fact(
          "In flight",
          plural(flying.length, "request") + ", the oldest for " + span(at - flying[0].since),
          flying.map((f) => "Request " + f.request + " for " + span(at - f.since)).join(", "),
        )
      : null,
    model && model.dropped_headers.length ? fact("Dropped request headers", model.dropped_headers.join(", ") + (model.more_dropped_headers ? ", and more" : ""), "The request headers that the route left behind") : null,
    data.other_build ? fact("Made by", "pumpkin " + data.other_build) : null,
  );
}

// The command that runs the run again from here, with a copy button, or why there is none; nothing
// if the reply has no `again`.
/** @param {HTMLElement} node @param {Run} data */
function drawAgain(node, data) {
  node.hidden = data.again === undefined;
  if (data.again === undefined) return;
  const command = data.again.command;
  if (command === undefined) return fill(node, h("span", { class: "name" }, "Again"), h("span", { class: "why", title: data.again.error ?? "" }, "-"));
  fill(
    node,
    h("span", { class: "name" }, "Again"),
    h("code", { title: command }, command),
    h("button", { title: "Copy the command that runs this run again", onclick: () => navigator.clipboard && navigator.clipboard.writeText(command) }, "Copy"),
  );
}

// A link to the trace of each caller with model answers: the agent, each call.
/** @param {HTMLElement} node @param {Run} data */
function drawTraces(node, data) {
  const callers = (data.lanes || []).filter((lane) => (lane.kind === "agent" || lane.kind === "target") && lane.answers && lane.answers.count > 0);
  fill(
    node,
    callers.length ? h("span", { class: "name" }, "Traces") : null,
    callers.map((lane) =>
      h(
        "a",
        { href: "#/run/" + encodeURIComponent(data.id) + "/trace?caller=" + encodeURIComponent(lane.caller ?? ""), class: "kind " + lane.kind, title: "What " + lane.lane + " and the model said to each other: " + plural(lane.answers?.count ?? 0, "answer") },
        lane.lane,
      ),
    ),
  );
}

// The lanes of the run's sandboxes, each from when the core asked for it until it was gone: its
// start faint, its life solid, a lease's commands and transfers on it, and a tick at each model
// answer. A sandbox that isn't gone runs to the last time the record knows, its end open.
/** @param {HTMLElement} node @param {Run} data @param {{ from: number | null, to: number | null, lit: Lit | null }} view @param {string | null} picked @param {(change: Record<string, string | null>) => void} change @param {(lane: Lane) => void} inspect */
function drawLanes(node, data, view, picked, change, inspect) {
  const times = data.times;
  if (!data.lanes || !times) {
    node.replaceChildren(h("p", { class: "empty" }, "The run's log doesn't record its start yet."));
    return;
  }
  const known = times.end ?? times.now ?? times.last;
  const span = Math.max(known, ...data.lanes.map((lane) => lane.sandbox.gone || 0), 1);
  const at = (/** @type {number} */ seconds) => (100 * seconds) / span + "%";
  const width = (/** @type {number} */ from, /** @type {number} */ to) => Math.max(0.15, (100 * (to - from)) / span) + "%";
  const ticks = [];
  for (let index = 0; index <= 4; index++) ticks.push(h("span", { style: { left: index * 25 + "%" } }, clock((span * index) / 4)));
  // A lane's name says its role and the role's place; the lane's own place, which can name its
  // GPU, is in its title and the inspector.
  const labels = data.lanes.map((lane) => {
    const role = laneRole(lane, data);
    const name = lane.lane + (role && role !== lane.lane ? " · " + role : "");
    const placed = data.talk && data.talk.roles.find((r) => r.name === role);
    return { name, place: (placed && placed.place) || lane.sandbox.place };
  });
  const widest = Math.max(...labels.map(({ name, place }) => name.length + 1 + place.length));
  node.style.setProperty("--label-chars", String(Math.min(LABEL_MAX_CHARS, Math.max(LABEL_MIN_CHARS, widest))));
  const across = (/** @type {number} */ fraction) => "calc(var(--label) + (100% - var(--label)) * " + fraction + ")";
  const rows = data.lanes.map((lane, index) => {
    const { asked, started, gone } = lane.sandbox;
    const end = gone ?? known;
    const parts = [];
    if (started !== null) {
      parts.push(h("div", { class: "span starting", style: { left: at(asked), width: width(asked, started) } }));
      parts.push(h("div", { class: "span" + (gone === null ? " open" : ""), style: { left: at(started), width: width(started, end) }, title: lane.said }));
    } else {
      parts.push(h("div", { class: "span starting" + (gone === null ? " open" : ""), style: { left: at(asked), width: width(asked, end) }, title: lane.said }));
    }
    for (const work of lane.work || []) {
      const failed = work.end && (work.end.failed || (work.end.exited && work.end.exited.code !== 0) || typeof work.end === "string");
      parts.push(h("div", { class: "work" + (failed ? " failed" : ""), style: { left: at(work.from), width: width(work.from, work.to ?? known) }, title: work.kind + " " + work.label }));
    }
    const failed = new Set(lane.failed || []);
    for (const answered of lane.answered || []) parts.push(h("div", { class: "tick" + (failed.has(answered) ? " failed" : ""), style: { left: at(answered) } }));
    const role = laneRole(lane, data);
    const { name, place } = labels[index];
    const title = lane.lane + (role ? " of role " + role : "") + (lane.of ? ", " + calls(lane.of) : "") + (lane.grant ? ", grant " + lane.grant : "") + ", on " + lane.sandbox.place;
    const lit = view.lit;
    const classes = ["lane", lane.kind, picked === lane.lane ? "on" : "", lit && !lit.lanes.has(lane.lane) ? "dim" : "", lit && lit.here !== null && lit.lanes.has(lane.lane) ? "lit" : ""];
    const pick = () => (change({ lane: picked === lane.lane ? null : lane.lane, role: null, sel: null }), inspect(lane));
    return h("div", { class: classes.join(" ") }, h("div", { class: "name", title, onclick: pick }, name, " ", h("span", { class: "place" }, place)), h("div", { class: "track" }, parts));
  });
  const axis = h("div", { class: "axis" }, h("div", { class: "ticks" }, ticks));
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
      role.final_target ? [h("dt", {}, "Final call"), h("dd", {}, "The final call's target, in which the body whose return is the run's result runs")] : null,
      h("dt", {}, "Model route"),
      routes(role),
    ),
  );
}

/** @param {Lane} lane @param {HTMLElement} inspector @param {(lane: string, time: number) => void} nearest @param {Run} data */
function inspectLane(lane, inspector, nearest, data) {
  const { place, asked, started, gone, deleted } = lane.sandbox;
  const at = (/** @type {number | null} */ seconds) => h("dd", { title: utc(data.times && data.times.started_unix, seconds) }, clock(seconds));
  const work = (lane.work || []).map((w) =>
    h("li", {}, h("button", { class: "mono", title: "Select the record nearest the start of this", onclick: () => nearest(lane.lane, w.from) }, clock(w.from) + "–" + clock(w.to) + " " + w.kind + " " + w.label + "  " + (w.end ? JSON.stringify(w.end) : "running"))),
  );
  /** @type {Array<[string, string | null | undefined]>} */
  const named = [
    ["Role", lane.role],
    ["Calls", lane.of && calls(lane.of)],
    ["Grant", lane.grant],
    ["Held by", lane.holder],
  ];
  const answers = lane.answers || { count: 0, failed: 0 };
  fill(
    inspector,
    h("h3", {}, h("span", { class: "kind " + lane.kind }, lane.kind), " ", lane.lane),
    h("p", { class: "said" }, lane.said),
    h(
      "dl",
      {},
      named.filter(([, value]) => value).flatMap(([name, value]) => [h("dt", {}, name), h("dd", {}, value)]),
      h("dt", {}, "Place"),
      h("dd", {}, place),
      h("dt", {}, "Asked"),
      at(asked),
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
  );
}

// Shows `record` in the inspector: its kind and time, its fields that are no object, the buttons
// `links`, a link to the payload it names, if any, then the record whole, as the log holds it,
// its strings cut, coloured.
/** @param {LogRecord} record @param {HTMLElement} inspector @param {Array<HTMLElement | false | null | undefined>} links @param {Run | null} data */
function inspectRecord(record, inspector, links, data) {
  const [kind] = saidOf(record);
  const started = data && data.times && data.times.started_unix;
  const { byte, when, ...kept } = record;
  const event = /** @type {Record<string, any>} */ (record.event || {});
  const payload = data && payloadOf(record);
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
    h("dl", {}, scalars.filter(([, value]) => value !== undefined && value !== "").flatMap(([name, value]) => [h("dt", {}, name), h("dd", {}, String(value))])),
    links.some(Boolean) ? h("p", { class: "links" }, links) : null,
    payload && data ? payloadLink(data.id, payload) : null,
    jsonText(kept),
  );
}
