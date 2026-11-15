// What the page's views share: making elements, the address, reading the server or the files of
// an export, following a live run, and the units numbers and times are shown in. Text from a
// record is only ever set as text, by `h` and `svg`, never as markup.
//
// Every word or phrase of the interface (a tab, a button, a menu's option, a column's head, a
// field's name, a state, a title, a hover's text) is in sentence case: its first letter a capital,
// the rest in lower case. A name from the records or the code keeps its spelling: a run's id, a
// caller, a role, a record's kind, a tool, a unit (`tok/s`), an acronym (`TTFT`, `API`).

// Whether the page reads the files that `pumpkin view --export` writes, from a host that serves
// them as they are, in place of a server's replies: its `index.html` says so.
export const STATIC = document.documentElement.dataset.source === "static";

// How long after a read's reply the next poll of a live run goes. In the static mode it is the
// lifetime that a host caches a changing file for, which `crates/pumpkin/src/publish.rs` sets.
export const POLL_MS = STATIC ? 30000 : 1500;

// How long after a read that failed to arrive the next goes, and the most that each wait doubles
// to while reads go on failing. Each read through a lost controller opens a connection to it
// anew, so seldom enough that one is done before the next starts; the first soon, so that the
// page resumes soon after a dropped connection, which is routine.
const AGAIN_FIRST_MS = 5000;
const AGAIN_MOST_MS = 60000;

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

// A read whose reply didn't arrive whole: the connection failed, or ended before or during the
// reply.
export class Unarrived extends Error {}

// The `Unarrived` of a read of `path` that failed with `error`; an abort stays what it is.
/** @param {string} path @param {unknown} error */
function unarrived(path, error) {
  if (error instanceof DOMException && error.name === "AbortError") return error;
  return new Unarrived("Reading " + path + " failed: " + (error instanceof Error ? error.message : String(error)));
}

// The JSON at `path`; a `Refused` if the server refuses it, an `Unarrived` if its reply doesn't
// arrive whole. `signal` aborts the read.
/** @param {string} path @param {AbortSignal} [signal] @returns {Promise<any>} */
export async function get(path, signal) {
  if (STATIC) {
    const key = fileOf(path);
    const response = await readFile(key, signal);
    // A file cut short, whose JSON doesn't parse, didn't arrive whole.
    const body = await response.json().catch((error) => {
      throw unarrived(key, error);
    });
    if (key === "runs.json" || /^runs\/[^/]+\/run\.json$/.test(key)) noteHead(response, key === "runs.json" ? body.runs.some((/** @type {any} */ run) => run.error === undefined && run.live) : body.live);
    return body;
  }
  const response = await fetch(path, { cache: "no-store", headers: { "X-Pumpkin-Token": TOKEN }, signal }).catch((error) => {
    throw unarrived(path, error);
  });
  if (!response.ok) {
    const refusal = await response.json().catch(() => ({}));
    throw new Refused(response.status, refusal.error || response.status + " " + response.statusText);
  }
  // An answer cut short, whose JSON doesn't parse, didn't arrive whole.
  return response.json().catch((error) => {
    throw unarrived(path, error);
  });
}

// The bytes at `path` as text, as the server keeps them; a `Refused` if it refuses them.
/** @param {string} path @returns {Promise<string>} */
export async function getText(path) {
  if (STATIC) return (await readFile(fileOf(path))).text();
  const response = await fetch(path, { cache: "no-store", headers: { "X-Pumpkin-Token": TOKEN } });
  const text = await response.text();
  if (!response.ok) throw new Refused(response.status, response.status + " " + response.statusText);
  return text;
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

// ---- the files of an export, which the page reads in its static mode

// The file of an export that holds the reply to `path`, a request of the server's API, by the
// keys that the module doc of `crates/pumpkin/src/view/export.rs` lists: their one home.
/** @param {string} path @returns {string} */
function fileOf(path) {
  const url = new URL(path, location.origin);
  const q = url.searchParams;
  const [api, runs, id, ...rest] = url.pathname.split("/").filter(Boolean).map(decodeURIComponent);
  const what = rest.join("/");
  if (api === "api" && runs === "runs") {
    if (id === undefined && !q.has("before")) return "runs.json";
    const run = "runs/" + id + "/";
    if (what === "") return run + "run.json";
    if (what === "log") return run + "log/" + q.get("at") + ".json";
    if (what === "diff") return run + "diff/" + q.get("a") + "-" + q.get("b") + "-" + q.get("part") + ".json";
    if (rest[0] === "item" && rest.length === 2) return run + "kept/items/" + rest[1];
    if (rest[0] === "exchange" && rest.length === 2) return run + "exchange/" + rest[1] + ".json";
    if (rest[0] === "exchange" && rest[2] === "response") return run + "kept/" + rest[1] + ".response";
    if (rest[0] === "exchange" && rest[2] === "request") return run + "kept/" + rest[1] + ".request";
    const trace = run + "trace/" + rest[1] + "/";
    if (rest[0] === "trace" && rest.length === 3) {
      if ((rest[2] === "steps" && !q.has("since")) || rest[2] === "ops") return trace + rest[2] + ".json";
      if (rest[2] === "stats") return trace + "stats-" + q.get("by") + ".json";
      if (rest[2] === "entry") return trace + "entry/" + q.get("at") + ".json";
      if (rest[2] === "entries" && q.get("from") === q.get("to") && !q.has("kind") && !q.has("thread")) return trace + "entries/" + q.get("from") + "-" + (q.get("entry") || "0") + ".json";
    }
  }
  throw new Error("No file of the export answers " + path);
}

// The export's file at `key`, as its host serves it, inflated by the browser; a `Refused` (404)
// where the host has none, which it answers by 403 or 404: a file not yet published, or not yet
// in the cache the page reads through; an `Unarrived` if the host can't be reached. A file that is
// never rewritten is read from the browser's cache; any other is asked again of the host, which
// answers "not modified" if it is.
/** @param {string} key @param {AbortSignal} [signal] */
export async function readFile(key, signal) {
  const immutable = /^runs\/[^/]+\/(exchange|diff|kept)\//.test(key);
  const response = await fetch(key, { cache: immutable ? "default" : "no-cache", signal }).catch((error) => {
    throw unarrived(key, error);
  });
  if (response.status === 403 || response.status === 404) throw new Refused(404, "Not published yet");
  if (!response.ok) throw new Refused(response.status, response.status + " " + response.statusText);
  return response;
}

// The view's head, `runs.json` or a run's `run.json`, as last read: when it was published, and
// whether a run it shows is live.
/** @type {{ at: number, live: boolean } | null} */
let head = null;
// A live run is published at the interval that `crates/pumpkin/src/publish.rs` sets: a head older
// than three of them says that the publisher may have stopped.
const STALE_MS = 3 * 60 * 1000;
// Whether a head read at `now`, in ms of the Unix clock, is of a publisher that may have stopped.
/** @param {{ at: number, live: boolean }} head @param {number} now */
const stale = (head, now) => head.live && now - head.at > STALE_MS;

/** @param {Response} response @param {boolean} live */
function noteHead(response, live) {
  const modified = response.headers.get("Last-Modified");
  head = modified ? { at: Date.parse(modified), live } : null;
  drawHead();
}

// A time ago in its largest whole unit: `40 s`, `7 min`, `3 h`.
/** @param {number} ms */
function ago(ms) {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  return seconds < 60 ? seconds + " s" : seconds < 3600 ? Math.floor(seconds / 60) + " min" : Math.floor(seconds / 3600) + " h";
}

// The line in the bar that says when what the view shows was published, kept current.
function drawHead() {
  const line = byId("published");
  if (!head) return line.replaceChildren();
  const now = Date.now();
  const when = new Date(head.at).toISOString().slice(11, 19);
  const late = stale(head, now) ? h("span", { class: "warn" }, ": nothing since, so the controller may have stopped, and what shows as live may have ended") : null;
  fill(line, late ? h("span", { class: "mark warn" }) : null, "Published at " + when + " UTC, " + ago(now - head.at) + " ago", late);
}
if (STATIC) setInterval(drawHead, 1000);

// ---- following

// What the page says of `error`, a read's, if a later read may well arrive, which it then makes:
// a reply that didn't arrive whole, or a 502, which the server answers alone for a controller that
// it can't reach, and a host for a store behind it that it can't. Null for an answer, such as a
// refusal of a run that doesn't exist, which the page shows.
/** @param {unknown} error @returns {string | null} */
function unreached(error) {
  if (STATIC) return error instanceof Unarrived || (error instanceof Refused && error.status === 502) ? "This page couldn't be reached, so it shows what it read last." : null;
  if (error instanceof Unarrived) return "A reply of this pumpkin view's was cut short or never came, so this page shows what it read last.";
  if (error instanceof Refused && error.status === 502) return "Can't reach the controller, so this page shows what it read last.";
  return null;
}

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
  if (STATIC) (head = null), drawHead();
  // Whether a read of this view has ended without failing.
  let drawn = false;
  const current = () => following === mine;
  // The wait after the next read that fails to arrive.
  let againMs = AGAIN_FIRST_MS;
  const again = async () => {
    if (!current()) return;
    if (document.hidden) {
      document.addEventListener("visibilitychange", again, { once: true });
      return;
    }
    /** @type {number | null} */
    let wait = null;
    const notice = byId("notice");
    try {
      wait = await read(current);
      notice.hidden = true;
      againMs = AGAIN_FIRST_MS;
      drawn = true;
    } catch (error) {
      const said = unreached(error);
      if (current() && said !== null) {
        wait = againMs;
        againMs = Math.min(2 * againMs, AGAIN_MOST_MS);
        notice.textContent = said + " It tries again in " + wait / 1000 + " s. " + sentence(error instanceof Error ? error.message : String(error));
        notice.hidden = false;
      } else if (STATIC && error instanceof Refused && error.status === 404) {
        // A run not yet published, or a file that a newer file names and the cache doesn't hold
        // yet: read again at the next poll, the view as it was until then. The host answered, so
        // a notice that it couldn't be reached goes.
        notice.hidden = true;
        againMs = AGAIN_FIRST_MS;
        if (current() && !drawn) show(h("p", { class: "empty" }, "Not published yet: read again every " + POLL_MS / 1000 + " s."));
        wait = POLL_MS;
      } else if (current()) show(error instanceof Refused && error.status === 401 ? unauthorized() : h("p", { class: "error" }, "Reading the record failed: " + (error instanceof Error ? error.message : String(error))));
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

// Dollars: `$12.34` from $1, three significant digits below, as `$0.0412`; `-` for a cost the
// record doesn't give.
/** @param {number | null | undefined} usd */
export function usd(usd) {
  if (usd === null || usd === undefined) return "-";
  if (usd >= 1 || usd === 0) return "$" + usd.toFixed(2);
  // As many decimals as the place of the first digit that isn't 0, and two more.
  return "$" + usd.toFixed(2 - Math.floor(Math.log10(usd)));
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

// A time of the run's clock as the record writes it, decimal nanoseconds, in seconds; null for
// one the record doesn't give.
/** @param {string | null | undefined} nanos */
export const seconds = (nanos) => (nanos === null || nanos === undefined ? null : Number(nanos) / 1e9);

// A setting of this browser's, such as a panel's width, which the page works without: storage
// can be refused, as in a private window.
/** @param {string} name @returns {string | null} */
export function setting(name) {
  try {
    return localStorage.getItem("pumpkin." + name);
  } catch {
    return null;
  }
}
/** @param {string} name @param {string} value */
export function keepSetting(name, value) {
  try {
    localStorage.setItem("pumpkin." + name, value);
  } catch {
    // Not kept: the setting is the default next time.
  }
}
