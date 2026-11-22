// The runs, newest first, a page of them at a time, as `GET /api/runs?before=<id>` gives them;
// the first page read again while a run in it is live, each row's cells changed where they differ,
// so that a selection of a row's text stays.

import { POLL_MS, count, fill, follow, get, h, plural, sentence, setKeys, show, tone, typing } from "./h.js";

/**
 * @typedef {{ runs: Array<ListedRun | Unlisted>, next: string | null }} Runs
 * @typedef {object} ListedRun a run as `ls` lists it
 * @property {string} id
 * @property {string} [member] the member whose job the run is, on a controller whose job names one
 * @property {boolean} live
 * @property {string} state "unread" for a run that another build made, whose page this build can't draw
 * @property {boolean} has_result
 * @property {string} time
 * @property {string} bound
 * @property {string} usd
 * @property {string} rest
 * @property {number | null} requests
 * @property {Tokens | null} tokens
 * @property {undefined} [error]
 * @typedef {{ id: string, error: string }} Unlisted a run whose record can't be read
 * @typedef {{ input: number | null, output: number | null, cached: number | null }} Tokens
 */

const COLUMNS = [
  ["State", ""],
  ["Task", ""],
  ["Run", ""],
  ["Time", "num"],
  ["Bound", ""],
  ["Cost", "num"],
  ["Requests", "num"],
  ["Tokens in", "num"],
  ["Tokens out", "num"],
  ["", "rest"],
];

// After the run's id, when some run names its member.
const MEMBER = ["Member", ""];

// A run's `rest`, as `ls` writes it, in two: its task, a word before the first ": ", and what
// follows it; a `rest` that begins with no such word, as a sentence does, is all the second.
/** @param {string} rest @returns {[string, string]} */
function taskOf(rest) {
  const at = rest.indexOf(": ");
  const task = at < 0 ? rest : rest.slice(0, at);
  if (task === "-" || /\s/.test(task)) return ["", rest];
  return [task, at < 0 ? "" : rest.slice(at + 2)];
}

// A cell's class: `none` added for a count of nothing or one unknown, which the style fades.
/** @param {string} cls @param {string} text */
const faded = (cls, text) => (text === "-" || text === "0" || text === "" ? cls + " none" : cls);

export async function runsView() {
  const body = h("tbody");
  const more = h("div", { class: "more" });
  const head = h("tr", {});
  const table = h("table", { class: "runs" }, h("thead", {}, head), body);
  // Whether the columns hold the member's, as `ls` shows it only when some run names one.
  /** @type {boolean | null} */
  let membered = null;
  const columns = () => (membered ? [...COLUMNS.slice(0, 3), MEMBER, ...COLUMNS.slice(3)] : COLUMNS);
  /** @type {{ first: Array<ListedRun | Unlisted>, older: Array<ListedRun | Unlisted>, next: string | null, cursor: number }} */
  const state = { first: [], older: [], next: null, cursor: -1 };
  // Each run's row, by its id, made once and changed after.
  /** @type {Map<string, HTMLTableRowElement>} */
  const rows = new Map();
  const open = (/** @type {string} */ id) => (location.hash = "#/run/" + encodeURIComponent(id));
  // Whether a run's page can be opened: not for one whose record can't be read, or is another
  // build's.
  const opens = (/** @type {ListedRun | Unlisted} */ run) => run.error === undefined && run.state !== "unread";

  // A row's cells: each its class, title and text, and the state's its mark.
  /** @typedef {{ cls: string, title: string | null, text: string, mark?: string, span?: number }} Cell */
  const cells = (/** @type {ListedRun | Unlisted} */ run) => {
    if (run.error !== undefined) {
      return /** @type {Cell[]} */ ([
        { cls: "", title: null, text: "" },
        { cls: "task", title: null, text: "" },
        { cls: "id", title: null, text: run.id },
        { cls: "error", title: null, text: run.error, span: columns().length - 3 },
      ]);
    }
    const listed = /** @type {ListedRun} */ (run);
    const tokens = listed.tokens;
    const noLog = "Unknown: the run's log says nothing of its model requests yet";
    const noTokens = "Unknown: no response reported its tokens";
    const counted = (/** @type {number | null | undefined} */ n) => (n === null || n === undefined ? (tokens === null ? noLog : noTokens) : null);
    const [task, said] = taskOf(listed.rest);
    // A count alone: its unit is its column's head.
    const number = (/** @type {number | null | undefined} */ n) => count(n, "").trimEnd();
    return /** @type {Cell[]} */ ([
      { cls: "state", title: null, text: sentence(listed.state), mark: "mark " + tone(listed) },
      { cls: "task", title: task || null, text: task },
      { cls: "id", title: null, text: listed.id },
      ...(membered ? [{ cls: "", title: null, text: listed.member ?? "-" }] : []),
      { cls: "num", title: null, text: listed.time },
      { cls: faded("mono", listed.bound), title: null, text: listed.bound },
      { cls: faded("num", listed.usd), title: null, text: listed.usd },
      { cls: faded("num", number(listed.requests)), title: listed.requests === null ? noLog : null, text: number(listed.requests) },
      { cls: faded("num", number(tokens && tokens.input)), title: counted(tokens && tokens.input), text: number(tokens && tokens.input) },
      { cls: faded("num", number(tokens && tokens.output)), title: counted(tokens && tokens.output), text: number(tokens && tokens.output) },
      { cls: "rest", title: said || null, text: said },
    ]);
  };
  // Sets `value` as `node`'s attribute `name`, null removing it, only where it differs.
  const attr = (/** @type {Element} */ node, /** @type {string} */ name, /** @type {string | null} */ value) => {
    if (value === null) node.removeAttribute(name);
    else if (node.getAttribute(name) !== value) node.setAttribute(name, value);
  };
  // Run `run`'s row, at `index`, made or changed where it differs from what it shows.
  const row = (/** @type {ListedRun | Unlisted} */ run, /** @type {number} */ index) => {
    const want = cells(run);
    let tr = rows.get(run.id);
    if (!tr || tr.children.length !== want.length) {
      tr = /** @type {HTMLTableRowElement} */ (h("tr", { onclick: () => opens(run) && open(run.id) }));
      for (const cell of want) tr.append(h("td", {}, cell.mark !== undefined ? h("span") : null, ""));
      rows.set(run.id, tr);
    }
    const listed = opens(run) ? /** @type {ListedRun} */ (run) : null;
    attr(tr, "class", (listed ? "link" : "unread") + (index === state.cursor ? " on" : ""));
    attr(tr, "title", listed ? (listed.live ? "Live: open it to follow it" : "Open this run") : null);
    want.forEach((cell, at) => {
      const td = /** @type {HTMLElement} */ (tr.children[at]);
      attr(td, "class", cell.cls || null);
      attr(td, "title", cell.title);
      attr(td, "colspan", cell.span === undefined ? null : String(cell.span));
      if (cell.mark !== undefined) attr(/** @type {Element} */ (td.firstElementChild), "class", cell.mark);
      const text = /** @type {Text} */ (td.lastChild);
      if (text.data !== cell.text) text.data = cell.text;
    });
    return tr;
  };
  const draw = () => {
    const runs = [...state.first, ...state.older];
    if (!runs.length) return show(h("p", { class: "empty" }, "No runs here yet."));
    const members = runs.some((run) => run.error === undefined && run.member !== undefined);
    if (members !== membered) {
      membered = members;
      head.replaceChildren(...columns().map(([name, cls]) => h("th", { class: cls }, name)));
    }
    const shown = runs.map(row);
    // Rows are moved only where the order changed, and the rows of unlisted runs removed.
    shown.forEach((tr, index) => {
      if (body.children[index] !== tr) body.insertBefore(tr, body.children[index] || null);
    });
    while (body.children.length > shown.length) /** @type {Element} */ (body.lastElementChild).remove();
    const ids = new Set(runs.map((run) => run.id));
    for (const id of rows.keys()) if (!ids.has(id)) rows.delete(id);
    fill(
      more,
      h("span", { class: "empty" }, plural(runs.length, "run") + (state.next ? " shown, the newest; older ones left out " : "")),
      state.next ? h("button", { title: "Read the next page of runs, those older than the last one shown", onclick: older }, "Older runs") : null,
    );
    if (!table.isConnected) show(table, more);
    body.children[state.cursor]?.scrollIntoView({ block: "nearest" });
  };
  const older = async () => {
    const last = state.older.length ? state.older[state.older.length - 1] : state.first[state.first.length - 1];
    /** @type {Runs} */
    let page;
    try {
      page = await get("/api/runs?before=" + encodeURIComponent(last.id));
    } catch (error) {
      more.append(h("span", { class: "error" }, " Reading them failed: " + (error instanceof Error ? error.message : String(error))));
      return;
    }
    state.older.push(...page.runs);
    state.next = page.next;
    draw();
  };
  setKeys((event) => {
    if (typing()) return;
    const runs = [...state.first, ...state.older];
    const step = { j: 1, ArrowDown: 1, k: -1, ArrowUp: -1 }[event.key];
    if (step && runs.length) {
      event.preventDefault();
      state.cursor = Math.max(0, Math.min(runs.length - 1, state.cursor + step));
      draw();
    } else if (event.key === "Enter" && runs[state.cursor] && opens(runs[state.cursor])) {
      open(runs[state.cursor].id);
    } else if (event.key === "Escape") {
      state.cursor = -1;
      draw();
    }
  });
  follow(async (current) => {
    /** @type {Runs} */
    const page = await get("/api/runs");
    if (!current()) return null;
    // The older pages start below the first page's last run: once that is another, a run came or
    // went, and they would skip or repeat one, so they are dropped, to be read again from here.
    const last = (/** @type {Array<ListedRun | Unlisted>} */ runs) => (runs.length ? runs[runs.length - 1].id : null);
    if (state.older.length && last(page.runs) !== last(state.first)) state.older = [];
    state.first = page.runs;
    if (!state.older.length) state.next = page.next;
    draw();
    return page.runs.some((run) => run.error === undefined && run.live) ? POLL_MS : null;
  });
}
