// What the page's views share: making elements, the address, reading the server, following a
// live run, and the units numbers and times are shown in. Text from a record is only ever set as
// text, by `h` and `svg`, never as markup.
//
// Every word or phrase of the interface (a tab, a button, a menu's option, a column's head, a
// field's name, a state, a title, a hover's text) is in sentence case: its first letter a capital,
// the rest in lower case. A name from the records or the code keeps its spelling: a run's id, a
// caller, a role, a record's kind, a tool, a unit (`tok/s`), an acronym (`TTFT`, `API`).

// How long after a read's reply the next poll of a live run goes.
export const POLL_MS = 1500;

/** @typedef {string | number | Node | null | undefined | false} Child */
/** @typedef {Record<string, any>} Attrs */

// The element of `index.html` whose id is `id`.
/** @param {string} id */
export const byId = (id) => /** @type {HTMLElement} */ (document.getElementById(id));

// Sets `attrs` on `node`: `on<event>` a listener, `style` an object of properties, set through
// the style object, as the page's policy refuses a `style` attribute; null, undefined and false
// none at all.
/** @param {Element} node @param {Attrs | null | undefined} attrs */
function set(node, attrs) {
  for (const [name, value] of Object.entries(attrs || {})) {
    if (value === null || value === undefined || value === false) continue;
    if (name.startsWith("on")) node.addEventListener(name.slice(2), value);
    else if (name === "style") {
      const style = /** @type {HTMLElement} */ (node).style;
      for (const [property, setTo] of Object.entries(value)) style.setProperty(property, String(setTo));
    } else node.setAttribute(name, value === true ? "" : String(value));
  }
}

/** @param {Element} node @param {Child[]} children */
function append(node, children) {
  for (const child of children.flat()) {
    if (child === null || child === undefined || child === false) continue;
    node.append(child instanceof Node ? child : String(child));
  }
}

/** @param {string} tag @param {Attrs | null} [attrs] @param {...(Child | Child[])} children @returns {HTMLElement} */
export function h(tag, attrs, ...children) {
  const node = document.createElement(tag);
  set(node, attrs);
  append(node, /** @type {Child[]} */ (children.flat()));
  return node;
}

/** @param {string} tag @param {Attrs | null} [attrs] @param {...(Child | Child[])} children @returns {SVGElement} */
export function svg(tag, attrs, ...children) {
  const node = document.createElementNS("http://www.w3.org/2000/svg", tag);
  set(node, attrs);
  append(node, /** @type {Child[]} */ (children.flat()));
  return node;
}

// Sets `node`'s children, leaving out each that is null: `replaceChildren` would show it as text.
/** @param {Element} node @param {...(Child | Child[])} children */
export function fill(node, ...children) {
  node.replaceChildren();
  append(node, /** @type {Child[]} */ (children.flat()));
}

/** @param {...(Child | Child[])} nodes */
export const show = (...nodes) => fill(byId("app"), ...nodes);

// ---- the address: `#/` the runs, `#/run/<id>` a run, with what is selected in its query

export function route() {
  const [path, query] = location.hash.slice(1).split("?");
  const parts = (path || "/").split("/").filter(Boolean).map(decodeURIComponent);
  return { parts, query: new URLSearchParams(query || "") };
}

// Sets `changes` in the address's query, `null` removing one, without adding to history.
/** @param {Record<string, string | number | null | undefined>} changes */
export function setQuery(changes) {
  const { parts, query } = route();
  for (const [name, value] of Object.entries(changes)) {
    if (value === null || value === undefined) query.delete(name);
    else query.set(name, String(value));
  }
  const path = "/" + parts.map(encodeURIComponent).join("/");
  const text = query.toString();
  history.replaceState(null, "", "#" + path + (text ? "?" + text : ""));
}

// ---- reading the server

// The token in the address that `pumpkin view` printed, which each request of the page's data
// carries.
const TOKEN = new URLSearchParams(location.search).get("t") || "";

// A refusal of the server's: its status, and its words.
export class Refused extends Error {
  /** @param {number} status @param {string} message */
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

// The JSON at `path`; a `Refused` if the server refuses it, a `SyntaxError` if its reply is cut
// short.
/** @param {string} path @returns {Promise<any>} */
export async function get(path) {
  const response = await fetch(path, { cache: "no-store", headers: { "X-Pumpkin-Token": TOKEN } });
  if (!response.ok) {
    const refusal = await response.json().catch(() => ({}));
    throw new Refused(response.status, refusal.error || response.status + " " + response.statusText);
  }
  return response.json();
}

// A POST to `path`: the status and the JSON that the server answers, which for a refusal is
// `{ error }`, or `{}` if it answers no JSON.
/** @param {string} path @returns {Promise<{ status: number, body: any }>} */
export async function post(path) {
  const response = await fetch(path, { method: "POST", cache: "no-store", headers: { "X-Pumpkin-Token": TOKEN } });
  return { status: response.status, body: await response.json().catch(() => ({})) };
}

// What the page says when the server refuses its token: a token cut short or run on by a copy of
// the address, or one of another `pumpkin view`, would else read as no runs.
function unauthorized() {
  return h(
    "div",
    {},
    h("p", { class: "error" }, TOKEN ? "The token in this page's address is not the one that this pumpkin view printed, so the page can't read the runs." : "This page's address has no token, so the page can't read the runs."),
    h("p", {}, "Copy the whole address that ", h("code", {}, "pumpkin view"), " printed, the line below the one that ends “at”, and open it."),
  );
}

// ---- following

/** @type {object | null} */
let following = null;

// Calls `read` now, and again as many ms after each read ends as it returns, until it returns
// null or another view is shown; a hidden page waits until it is shown. `read` is given whether
// its view is still the one shown, which it asks after each wait, before it draws. Returns that
// same question, for what else the view waits on.
/** @param {(current: () => boolean) => Promise<number | null>} read @returns {() => boolean} */
export function follow(read) {
  const mine = {};
  following = mine;
  const current = () => following === mine;
  const again = async () => {
    if (!current()) return;
    if (document.hidden) {
      document.addEventListener("visibilitychange", again, { once: true });
      return;
    }
    /** @type {number | null} */
    let wait = null;
    try {
      wait = await read(current);
    } catch (error) {
      if (current()) show(error instanceof Refused && error.status === 401 ? unauthorized() : h("p", { class: "error" }, "Reading the record failed: " + (error instanceof Error ? error.message : String(error))));
    }
    if (current() && wait !== null) setTimeout(again, wait);
  };
  again();
  return current;
}

// ---- the keys: each view sets what its keys do

/** @type {((event: KeyboardEvent) => void) | null} */
let keys = null;

/** @param {((event: KeyboardEvent) => void) | null} handler */
export const setKeys = (handler) => {
  keys = handler;
};

/** @param {KeyboardEvent} event */
export function key(event) {
  if (event.ctrlKey || event.metaKey || event.altKey) return;
  if (keys) keys(event);
}

// Whether the reader is typing into a field, where the keys are the field's.
export const typing = () => ["INPUT", "SELECT", "TEXTAREA"].includes(document.activeElement?.tagName ?? "");

// ---- units: every number with its unit, every time since the run started as `m:ss.s`

// `seconds` since the run started, as `m:ss.s`, or `h:mm:ss.s`; `-` for a time the record doesn't
// give.
/** @param {number | null | undefined} seconds */
export function clock(seconds) {
  if (seconds === null || seconds === undefined) return "-";
  const tenths = Math.floor(seconds * 10);
  const whole = Math.floor(tenths / 10);
  const [hours, minutes, secs] = [Math.floor(whole / 3600), Math.floor((whole % 3600) / 60), whole % 60];
  /** @param {number} n */
  const pad = (n) => String(n).padStart(2, "0");
  return (hours ? hours + ":" + pad(minutes) : minutes) + ":" + pad(secs) + "." + (tenths % 10);
}

// The UTC time `seconds` after `started`, a Unix time in seconds, as a time's hover says it.
/** @param {number | null | undefined} started @param {number | null | undefined} seconds */
export function utc(started, seconds) {
  if (started === null || started === undefined || seconds === null || seconds === undefined) return "";
  return new Date((started + seconds) * 1000).toISOString().replace("T", " ").replace("Z", " UTC");
}

// A count in three digits at most, with `unit`: `131k tok`, `8,468 req` below ten thousand.
/** @param {number | null | undefined} count @param {string} unit */
export function count(count, unit) {
  if (count === null || count === undefined) return "-";
  if (count < 10000) return count.toLocaleString("en-US") + " " + unit;
  const [scaled, prefix] = count < 1e6 ? [count / 1e3, "k"] : count < 1e9 ? [count / 1e6, "M"] : [count / 1e9, "G"];
  return (scaled < 100 ? scaled.toFixed(1) : scaled.toFixed(0)) + prefix + " " + unit;
}

// Bytes in three digits at most: `840 B`, `21.4 KB`, `4.72 MB`.
/** @param {number | null | undefined} bytes */
export function bytes(bytes) {
  if (bytes === null || bytes === undefined) return "-";
  if (bytes < 1000) return bytes + " B";
  const [scaled, unit] = bytes < 1e6 ? [bytes / 1e3, "KB"] : bytes < 1e9 ? [bytes / 1e6, "MB"] : [bytes / 1e9, "GB"];
  return scaled.toPrecision(3) + " " + unit;
}

// A duration in the unit that keeps three digits: `840 ms`, `3.76 s`, `12.4 min`.
/** @param {number | null | undefined} seconds */
export function span(seconds) {
  if (seconds === null || seconds === undefined) return "-";
  if (seconds < 1) return Math.round(seconds * 1000) + " ms";
  if (seconds < 100) return seconds.toPrecision(3) + " s";
  if (seconds < 6000) return (seconds / 60).toPrecision(3) + " min";
  return (seconds / 3600).toPrecision(3) + " h";
}

/** @param {number} count @param {string} noun */
export const plural = (count, noun) => count.toLocaleString("en-US") + " " + noun + (count === 1 ? "" : "s");

// The state a run's mark takes its colour from: a finish without a score is a warning, not an ok.
/** @param {{ live: boolean, state: string, scored: boolean }} run */
export const tone = (run) => (run.live ? "live" : run.state === "finished" && !run.scored ? "warn" : run.state);

// `text` with its first letter a capital, for a phrase that starts with a name from the records,
// such as a run's state.
/** @param {string} text */
export const sentence = (text) => text.charAt(0).toUpperCase() + text.slice(1);
