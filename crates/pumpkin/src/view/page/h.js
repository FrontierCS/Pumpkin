// What the page's views share: making elements, the address, reading the server, following a live
// run, the units of numbers and times, and the grips that size the panes. This browser keeps the
// panes' lengths. Text from a record is only ever set as text, by `h` and `svg`, never as markup.
//
// Every word or phrase of the interface (a tab, a button, a menu's option, a column's head, a
// field's name, a state, a title, a hover's text) is in sentence case: its first letter a capital,
// the rest in lower case. A name from the records or the code keeps its spelling: a run's id, a
// caller, a role, a record's kind, a tool, a unit (`tok/s`), an acronym (`TTFT`, `API`).

// How long after a read's reply the next poll of a live run goes.
export const POLL_MS = 1500;

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

// ---- a card of what matters less: what a part of the page holds back until the pointer or the
// focus is on it, drawn beside it, above everything else. A card takes no pointer, so it holds
// only what is read, never a control.

/** @type {WeakMap<Element, () => Child | Child[]>} */
const cards = new WeakMap();
/** @type {{ node: HTMLElement | null, of: Element | null }} */
const carded = { node: null, of: null };

// Gives `node` the card that `content` draws when it is asked for. Returns `node`.
/** @template {Element} T @param {T} node @param {() => Child | Child[]} content @returns {T} */
export function card(node, content) {
  cards.set(node, content);
  node.classList.add("carded");
  return node;
}

// The rows of a card: each name beside its value, and no row for a value that is null.
/** @param {Array<[string, Child] | null | false>} rows */
export const cardRows = (rows) =>
  h(
    "dl",
    {},
    rows.flatMap((row) => (row && row[1] !== null && row[1] !== undefined && row[1] !== "" ? [h("dt", {}, row[0]), h("dd", {}, row[1])] : [])),
  );

function hideCard() {
  carded.node?.remove();
  carded.node = carded.of = null;
}

// Shows the card of the nearest carded element at or above `target`, below it, or above it where
// the window ends first; none hides the card shown.
/** @param {EventTarget | null} target */
function showCard(target) {
  let of = target instanceof Element ? target : null;
  while (of && !cards.has(of)) of = of.parentElement;
  if (of === carded.of && (!of || carded.node?.isConnected)) return;
  hideCard();
  const content = of && cards.get(of);
  if (!of || !content) return;
  const node = h("div", { class: "card", role: "tooltip" }, content());
  // A card whose every row is empty has nothing to show.
  if (!node.textContent) return;
  document.body.append(node);
  const [at, box, gap] = [of.getBoundingClientRect(), node.getBoundingClientRect(), 6];
  const left = Math.max(8, Math.min(at.left, innerWidth - box.width - 8));
  const below = at.bottom + gap + box.height <= innerHeight - 8 || at.top - gap - box.height < 8;
  node.style.setProperty("left", left + "px");
  node.style.setProperty("top", (below ? at.bottom + gap : at.top - gap - box.height) + "px");
  [carded.node, carded.of] = [node, of];
}

addEventListener("mouseover", (event) => showCard(event.target));
addEventListener("focusin", (event) => showCard(event.target));
addEventListener("focusout", hideCard);
// What a card is of can be drawn anew under the pointer, or scrolled from under it.
addEventListener("mousemove", () => carded.of && !carded.of.isConnected && hideCard());
addEventListener("scroll", hideCard, true);
addEventListener("mousedown", hideCard);
document.documentElement.addEventListener("mouseleave", hideCard);

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

// A refusal of the server's: its status, its words, and, for a controller that is stopped, the
// path that starts it.
export class Refused extends Error {
  /** @param {number} status @param {string} message @param {string | null} [start] */
  constructor(status, message, start = null) {
    super(message);
    this.status = status;
    this.start = start;
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
  const response = await fetch(path, { cache: "no-store", headers: { "X-Pumpkin-Token": TOKEN }, signal }).catch((error) => {
    throw unarrived(path, error);
  });
  if (!response.ok) {
    const refusal = await response.json().catch(() => ({}));
    throw new Refused(response.status, refusal.error || response.status + " " + response.statusText, refusal.start);
  }
  // An answer cut short, whose JSON doesn't parse, didn't arrive whole.
  return response.json().catch((error) => {
    throw unarrived(path, error);
  });
}

// The bytes at `path` as text, as the server keeps them; a `Refused` if it refuses them.
/** @param {string} path @returns {Promise<string>} */
export async function getText(path) {
  const response = await fetch(path, { cache: "no-store", headers: { "X-Pumpkin-Token": TOKEN } });
  const text = await response.text();
  if (!response.ok) throw new Refused(response.status, response.status + " " + response.statusText);
  return text;
}

// The bytes at `path`, as the server keeps them; a `Refused` if it refuses them.
/** @param {string} path @returns {Promise<Uint8Array>} */
export async function getBytes(path) {
  const response = await fetch(path, { cache: "no-store", headers: { "X-Pumpkin-Token": TOKEN } }).catch((error) => {
    throw unarrived(path, error);
  });
  if (!response.ok) {
    const refusal = await response.json().catch(() => ({}));
    throw new Refused(response.status, refusal.error || response.status + " " + response.statusText);
  }
  return new Uint8Array(await response.arrayBuffer());
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

// What the page says of `error`, a read's, if a later read may well arrive. The page then makes
// that read. Such an error is a reply that didn't arrive whole, or a 502 or a 503. The server
// answers a 502 alone, for a controller that it can't reach, and a 503 for a controller that
// doesn't run. Null for an answer, such as a refusal of a run that doesn't exist, which the page
// shows.
/** @param {unknown} error @returns {string | null} */
function unreached(error) {
  if (error instanceof Unarrived) return "A reply of this pumpkin view's was cut short or never came, so this page shows what it read last.";
  if (error instanceof Refused && error.status === 502) return "Can't reach the controller, so this page shows what it read last.";
  if (error instanceof Refused && error.status === 503) return "The controller can't answer now, so this page shows what it read last.";
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
  const current = () => following === mine;
  // The wait after the next read that fails to arrive.
  let againMs = AGAIN_FIRST_MS;
  // The wait for the next read, while one is set.
  /** @type {ReturnType<typeof setTimeout> | null} */
  let timer = null;
  // Starts the controller by a `POST` to `path` from `button`, which is disabled meanwhile, and
  // then reads again after the first wait.
  /** @param {string} path @param {HTMLElement} button */
  const start = async (path, button) => {
    const notice = byId("notice");
    button.toggleAttribute("disabled", true);
    const started = await post(path).catch((error) => ({ status: 0, body: { error: String(error) } }));
    button.toggleAttribute("disabled", false);
    if (!current()) return;
    if (started.status !== 202) {
      fill(notice, "Starting the controller failed: " + sentence(started.body.error || "it answered " + started.status) + ". ", button);
      return;
    }
    notice.classList.remove("error");
    notice.textContent = sentence(started.body.said) + ": this page reads again in " + AGAIN_FIRST_MS / 1000 + " s.";
    // A read under way waits `againMs` once it fails; a wait that is set is cut to the first.
    if (timer === null) againMs = AGAIN_FIRST_MS;
    else {
      clearTimeout(timer);
      timer = setTimeout(again, AGAIN_FIRST_MS);
      againMs = Math.min(2 * AGAIN_FIRST_MS, AGAIN_MOST_MS);
    }
  };
  const again = async () => {
    timer = null;
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
    } catch (error) {
      const said = unreached(error);
      if (current() && said !== null) {
        wait = againMs;
        againMs = Math.min(2 * againMs, AGAIN_MOST_MS);
        const stopped = error instanceof Refused ? error.start : null;
        const button = stopped ? h("button", { title: "Start the controller's instance" }, "Start it") : null;
        if (stopped && button) button.addEventListener("click", () => start(stopped, button));
        notice.classList.add("error");
        fill(notice, said + " It tries again in " + wait / 1000 + " s. " + sentence(error instanceof Error ? error.message : String(error)), button ? [" ", button] : null);
        notice.hidden = false;
      } else if (current()) show(error instanceof Refused && error.status === 401 ? unauthorized() : h("p", { class: "error" }, "Reading the record failed: " + (error instanceof Error ? error.message : String(error))));
    }
    if (current() && wait !== null) timer = setTimeout(again, wait);
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

// Bytes in the largest binary unit that holds at least one of them, to a tenth cut short, as
// `pumpkin`'s `size` says them: `840 bytes`, `21.4 KiB`, `4.7 MiB`.
/** @param {number | null | undefined} bytes */
export function bytes(bytes) {
  if (bytes === null || bytes === undefined) return "-";
  /** @type {[number, string][]} */
  const units = [[2 ** 30, "GiB"], [2 ** 20, "MiB"], [2 ** 10, "KiB"]];
  const unit = units.find(([size]) => bytes >= size);
  if (unit === undefined) return bytes + (bytes === 1 ? " byte" : " bytes");
  const tenths = Math.floor((bytes * 10) / unit[0]);
  return Math.floor(tenths / 10) + "." + (tenths % 10) + " " + unit[1];
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

/**
 * @param {number} count
 * @param {string} noun
 * @param {string} [nouns] its plural, if not `noun` with an `s`
 */
export const plural = (count, noun, nouns = noun + "s") => count.toLocaleString("en-US") + " " + (count === 1 ? noun : nouns);

// The state a run's mark takes its colour from: a finish without a result is a warning, not an ok,
// and an unread run's mark is the plain one, as the class `unread` hides an empty element.
/** @param {{ live: boolean, state: string, has_result: boolean }} run */
export const tone = (run) => (run.live ? "live" : run.state === "finished" && !run.has_result ? "warn" : run.state === "unread" ? "" : run.state);

// `text` with its first letter a capital, for a phrase that starts with a name from the records,
// such as a run's state.
/** @param {string} text */
export const sentence = (text) => text.charAt(0).toUpperCase() + text.slice(1);

// A time of the run's clock as the record writes it, decimal nanoseconds, in seconds; null for
// one the record doesn't give.
/** @param {string | null | undefined} nanos */
export const seconds = (nanos) => (nanos === null || nanos === undefined ? null : Number(nanos) / 1e9);

// ---- grips: the panes' lengths, set by dragging and kept in this browser

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
/** @param {string} name */
function forgetSetting(name) {
  try {
    localStorage.removeItem("pumpkin." + name);
  } catch {
    // Not forgotten: the setting is kept next time.
  }
}

// How far a press of an arrow moves a grip, unless its pane comes in steps of its own.
const GRIP_STEP_PX = 16;
// The arrows that move a grip across columns (`x`) or rows (`y`), each to the left or up (-1), or
// to the right or down (1).
/** @type {Record<"x" | "y", Record<string, -1 | 1>>} */
const GRIP_ARROWS = { x: { ArrowLeft: -1, ArrowRight: 1 }, y: { ArrowUp: -1, ArrowDown: 1 } };

// A grip between two panes that sizes the pane `sized` by the CSS length `property` of `on`, kept
// as the setting `name`: by dragging it, by its arrows, `stepPx` a press, and back to the style's
// default by a double-click. `across` is "x" for a grip between columns, which sizes the column
// after it, and "y" for one between rows, which sizes the row before it. The style bounds the
// length.
/**
 * @param {{ name: string, across: "x" | "y", sized: HTMLElement, on: HTMLElement, property: "--side" | "--pane" | "--inspector", stepPx?: number, title: string }} spec
 * @returns {HTMLElement}
 */
export function grip({ name, across, sized, on, property, stepPx = GRIP_STEP_PX, title }) {
  const node = h("div", { class: "grip" + (across === "y" ? " across-y" : ""), tabindex: 0, role: "separator", "aria-orientation": across === "x" ? "vertical" : "horizontal", title: title + " (arrows too; double-click resets)" });
  // The column after the grip narrows as the grip moves right; the row before it grows as it
  // moves down.
  const grows = across === "x" ? -1 : 1;
  const set = (/** @type {number} */ px) => on.style.setProperty(property, Math.round(px) + "px");
  const lengthPx = () => (across === "x" ? sized.getBoundingClientRect().width : sized.getBoundingClientRect().height);
  const at = (/** @type {PointerEvent} */ event) => (across === "x" ? event.clientX : event.clientY);
  const measure = () => ({ px: lengthPx(), property: on.style.getPropertyValue(property) });
  // Keeps the length shown, if it differs from the one `before`; else puts the property back as it
  // was, so that a click, or a push against a bound, leaves a pane at its default following the
  // window as it is resized.
  const keepChanged = (/** @type {{ px: number, property: string }} */ before) => {
    const afterPx = Math.round(lengthPx());
    if (afterPx !== Math.round(before.px)) {
      set(afterPx);
      keepSetting(name, String(afterPx));
    } else if (before.property) on.style.setProperty(property, before.property);
    else on.style.removeProperty(property);
  };
  const keptPx = Number(setting(name));
  if (keptPx) set(keptPx);
  node.addEventListener("pointerdown", (down) => {
    node.setPointerCapture(down.pointerId);
    const [fromPx, before] = [at(down), measure()];
    const move = (/** @type {PointerEvent} */ moved) => set(before.px + grows * (at(moved) - fromPx));
    // The grip loses the pointer when it is let go and when the browser cancels the drag: either
    // ends the drag, at the length shown.
    const end = () => {
      node.removeEventListener("pointermove", move);
      keepChanged(before);
    };
    node.addEventListener("pointermove", move);
    node.addEventListener("lostpointercapture", end, { once: true });
  });
  node.addEventListener("dblclick", () => {
    on.style.removeProperty(property);
    forgetSetting(name);
  });
  node.addEventListener("keydown", (event) => {
    const way = GRIP_ARROWS[across][event.key];
    if (!way) return;
    // The arrow moves the grip, not the page.
    event.preventDefault();
    const before = measure();
    set(before.px + grows * way * stepPx);
    keepChanged(before);
  });
  return node;
}
