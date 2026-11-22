// What a run returned, for its reader: every value drawn by its shape, folded under a line that
// names it. Nothing here knows a value's name or meaning, which the workflow gives.

import { fill, h, keepSetting, plural, setting } from "./h.js";
import { jsonText } from "./payload.js";

// A number as `pumpkin ls` shows one: two decimals from 1 up, four below, no trailing zeros.
/** @param {number} number */
function shownNumber(number) {
  const shown = number.toFixed(Math.abs(number) >= 1 ? 2 : 4).replace(/0+$/, "").replace(/\.$/, "");
  return shown === "-0" ? "0" : shown;
}

// An object's value as the page shows it: a number as `pumpkin ls` does, with all its digits in its
// title; a word as itself.
/** @param {any} value */
function scalarView(value) {
  if (typeof value === "number") return h("span", { class: "num", title: String(value) }, shownNumber(value));
  return h("span", {}, typeof value === "string" ? value : String(value));
}

// An object or a list, drawn by its shape and not by its names: each value that isn't an object on
// a line, its name at the left; objects that hold only such values as a table, a row each, a
// column each name; any other object under its name, a level in. Past four levels, or a list of
// mixed things, it is the JSON.
/** @param {any} value @param {number} depth @returns {import("./h.js").Child | import("./h.js").Child[]} */
function objectView(value, depth) {
  const scalar = (/** @type {any} */ v) => v === null || typeof v !== "object";
  const flat = (/** @type {any} */ v) => !scalar(v) && !Array.isArray(v) && Object.values(v).every(scalar) && Object.keys(v).length > 0;
  if (Array.isArray(value) && value.every(scalar)) return h("div", { class: "items" }, value.map((item) => scalarView(item)));
  const entries = Array.isArray(value) ? value.map((item, index) => /** @type {[string, any]} */ ([String(index), item])) : Object.entries(value);
  if (depth > 3 || (Array.isArray(value) && !value.every(flat))) return jsonText(value);
  if (entries.length > 1 && entries.every(([, item]) => flat(item))) {
    const columns = [...new Set(entries.flatMap(([, item]) => Object.keys(item)))];
    return h(
      "table",
      { class: "grid" },
      h("thead", {}, h("tr", {}, h("th", {}), columns.map((column) => h("th", {}, column)))),
      h("tbody", {}, entries.map(([name, item]) => h("tr", {}, h("th", {}, name), columns.map((column) => h("td", {}, column in item ? scalarView(item[column]) : ""))))),
    );
  }
  // Every value on a line of its own, its name at the left and the values in one column; an
  // object under its name, a level in.
  return h(
    "div",
    { class: "kv" },
    // The lines first, then the objects, each kind in the order it came in.
    [...entries.filter(([, item]) => scalar(item)), ...entries.filter(([, item]) => !scalar(item))].flatMap(([name, item]) =>
      scalar(item) ? [h("span", { class: "k" }, name), scalarView(item)] : [h("span", { class: "group" }, name), h("div", { class: "nest" }, objectView(item, depth + 1))],
    ),
  );
}

// The page draws what the run returned by its shape, for its reader. The workflow gives the values
// meaning.
/** @typedef {{ values: Array<{ name: string, value: any }> }} Returned */

// What a run returned, open: every value by its shape.
/** @param {Returned} result */
export function returnedView(result) {
  return h("div", { class: "opened" }, objectView(Object.fromEntries(result.values.map(({ name, value }) => [name, value])), 0));
}

/** @param {HTMLElement} node @param {Returned | null | undefined} result */
export function drawResult(node, result) {
  if (!result) return fill(node);
  // Folded until it is asked for; the browser keeps whether it is open.
  const fold = /** @type {HTMLDetailsElement} */ (h("details", { class: "returned" }, h("summary", {}, "Result", h("span", { class: "hint" }, plural(result.values.length, "value"))), returnedView(result)));
  fold.open = setting("run.result") === "open";
  fold.addEventListener("toggle", () => keepSetting("run.result", fold.open ? "open" : "shut"));
  fill(node, fold);
}
