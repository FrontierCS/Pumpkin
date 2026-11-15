// A step's Diff tab: what its request changed against its parent's, the step before it in its
// thread: the items it added past the parent's list, and its system text and its tools as the
// server's `diff` gives them, a line diff in hunks, read only for a part the steps reply says
// differs.

import { fill, get, h, plural } from "./h.js";

/**
 * @typedef {import("./steps.js").Steps} Steps
 * @typedef {object} DiffReply `diff?a=<request>&b=<request>&part=<part>`
 * @property {boolean} [same] whether the two sent one part
 * @property {string} [unread] why a side can't be read
 * @property {string} [too_long] why the sides weren't compared
 * @property {[number, number]} [lines] each side's lines
 * @property {Array<{ at: [number, number], lines: Array<[" " | "-" | "+", string]> }>} [hunks] each at its first line's index in each side
 */

// The parts of a request that the server's diff compares, with the words for each.
const PARTS = [
  ["system", "System text"],
  ["tools", "Tool list"],
];

// The diff of `part` of requests `a` and `b`, drawn into a new element once read.
/** @param {string} base @param {number} a @param {number} b @param {string} part */
function drawn(base, a, b, part) {
  const node = h("div", { class: "diff" }, h("p", { class: "dim" }, "Reading the diff…"));
  get(base + "/diff?a=" + a + "&b=" + b + "&part=" + part).then(
    (/** @type {DiffReply} */ reply) => {
      if (reply.unread) return fill(node, h("p", { class: "error" }, "Not compared: " + reply.unread));
      if (reply.same) return fill(node, h("p", { class: "dim" }, "The same as its parent's"));
      const [from, to] = reply.lines || [0, 0];
      const sides = h("p", { class: "dim" }, plural(from, "line") + " in its parent's, " + plural(to, "line") + " in its own");
      if (reply.too_long) return fill(node, sides, h("p", { class: "error" }, reply.too_long));
      fill(
        node,
        sides,
        (reply.hunks || []).map((hunk) =>
          h(
            "pre",
            { class: "hunk" },
            h("span", { class: "at" }, "@@ line " + (hunk.at[0] + 1) + " → " + (hunk.at[1] + 1) + " @@"),
            hunk.lines.map(([sign, line]) => h("span", { class: sign === "+" ? "add" : sign === "-" ? "del" : "keep" }, sign + " " + line)),
          ),
        ),
      );
    },
    (error) => fill(node, h("p", { class: "error" }, "Reading the diff failed: " + (error instanceof Error ? error.message : String(error)))),
  );
  return node;
}

/** @param {Steps} steps @param {number} s @param {string} base */
export function diffTab(steps, s, base) {
  const c = steps.cols;
  const parent = c.parent[s];
  const at = parent === null ? undefined : steps.row(parent);
  if (parent === null || at === undefined) return h("p", { class: "empty" }, "The first step of its thread: no step before it to compare with.");
  const differs = c.differs[s] || [];
  return h(
    "div",
    {},
    h("h4", {}, "Items"),
    h("p", {}, c.added[s] === null ? "-" : plural(c.added[s] ?? 0, "item") + " added past its parent's " + plural(c.items[at] ?? 0, "item") + ", " + plural(c.items[s] ?? 0, "item") + " sent"),
    PARTS.flatMap(([part, name]) => [h("h4", {}, name), differs.includes(part) ? drawn(base, parent, c.request[s], part) : h("p", { class: "dim" }, "The same as its parent's")]),
  );
}
