// The run page's diagram of who talks to whom: its roles in layers, the lines between them, and
// what went along each.

import { bytes, card, cardRows, fill, h, plural, svg } from "./h.js";
import { talkMark } from "./failure.js";

/**
 * @typedef {import("./run.js").Run} Run
 * @typedef {import("./run.js").Lane} Lane
 * @typedef {import("./run.js").Role} Role
 * @typedef {import("./run.js").Edge} Edge
 * @typedef {import("./run.js").Lit} Lit
 * @typedef {import("./failure.js").Failure} Failure
 */

// The widths of a character of the diagram's names and places, in its font, `--fs-diagram`.
const NAME_CHAR_PX = 7.3;
// The diagram's end of the edges of model requests, the model upstream, which is no role.
const MODEL = "model";
const PLACE_CHAR_PX = 7.3;

/** @param {Lane} lane @param {Run} data */
export function laneRole(lane, data) {
  if (lane.kind === "agent" && !lane.role) {
    const agent = data.talk && data.talk.roles.find((role) => role.kind === "agent");
    return agent ? agent.name : null;
  }
  return lane.role ?? null;
}

// Who talks to whom, in layers left to right: a role sits one layer right of the furthest role
// that reaches it, by a grant of the IR or by what the log shows, so every line but a cycle's
// runs left to right and needs no arrow. In a layer, roles sit at the mean row of what reaches
// them, so a runner sits on its holder's row; the model upstream sits under the agent. A line
// leaves and enters a box level, at its own point of the box's side; a grant the log shows no
// use of is dashed, as is a role with no sandbox yet. A role of more than one sandbox is drawn
// as a stack, with how many it had and how many of them run now. Below, what went
// between them. What a selection lights is lit, the rest dim; a role picked lights its lines,
// and a click off every box puts the lighting down. The failure `failed` is marked on the line
// between its subject's two ends, if the diagram draws one, else on the box of its role.
// The layering is Sugiyama's, as Graphviz's `dot` draws a graph (Gansner et al., "A Technique
// for Drawing Directed Graphs", 1993), without its crossing reduction, for a handful of roles.
/** @param {HTMLElement} node @param {Run} data @param {Lit | null} lit @param {string | null} picked @param {Failure | null | undefined} failed @param {(name: string | null) => void} pick */
export function drawTalk(node, data, lit, picked, failed, pick) {
  if (!data.talk) return fill(node, h("p", { class: "empty" }, "The record has no roles yet."));
  const { roles, edges } = data.talk;
  const grants = data.talk.grants || [];
  /** @type {Record<string, number>} */
  const order = { agent: 0, target: 1, runner: 2 };
  const shown = roles.map((role, index) => ({ ...role, index }));
  if (roles.some((role) => role.routed) || edges.some((edge) => edge.to === MODEL)) {
    shown.push({ name: MODEL, kind: "model", place: "The route's upstream", routed: false, final_target: false, routes: [], index: roles.length });
  }
  const byName = new Map(shown.map((n) => [n.name, n]));
  /** @type {Map<string, Edge[]>} */
  const pairs = new Map();
  for (const edge of edges) {
    const key = edge.from + "\u0000" + edge.to;
    pairs.set(key, [...(pairs.get(key) || []), edge]);
  }
  // Each pair of roles that a grant or the log joins, once whichever way its traffic went, from
  // the grant's holder, or from the end that the log shows first.
  /** @type {Map<string, { from: string, to: string }>} */
  const links = new Map();
  for (const { from, to } of [...grants, ...edges]) {
    if (!byName.has(from) || !byName.has(to) || from === to || links.has(to + "\u0000" + from)) continue;
    links.set(from + "\u0000" + to, { from, to });
  }
  // What passed from `from` to `to`: each kind's count and its bytes there and back.
  const said = (/** @type {string} */ from, /** @type {string} */ to) =>
    (pairs.get(from + "\u0000" + to) || [])
      .map((edge) => {
        let text = plural(edge.count, edge.kind);
        if (edge.bytes !== null) text += " " + bytes(edge.bytes);
        if (edge.back_bytes !== null) text += ", " + bytes(edge.back_bytes) + " back";
        return text;
      })
      .join(" · ");
  const between = (/** @type {string} */ a, /** @type {string} */ b) => [...(pairs.get(a + "\u0000" + b) || []), ...(pairs.get(b + "\u0000" + a) || [])];
  // Layers: the longest path from a role nothing reaches, along the links but the model's and
  // those that close a cycle, which a depth-first search finds.
  /** @type {Map<string, string[]>} */
  const out = new Map();
  for (const { from, to } of links.values()) if (to !== MODEL) out.set(from, [...(out.get(from) || []), to]);
  const reached = new Set([...links.values()].filter((l) => l.to !== MODEL).map((l) => l.to));
  /** @type {Set<string>} */
  const back = new Set();
  /** @type {Map<string, "open" | "done">} */
  const seen = new Map();
  /** @param {string} name */
  const visit = (name) => {
    seen.set(name, "open");
    for (const to of out.get(name) || []) {
      if (seen.get(to) === "open") back.add(name + "\u0000" + to);
      else if (!seen.has(to)) visit(to);
    }
    seen.set(name, "done");
  };
  for (const n of shown) if (!reached.has(n.name)) visit(n.name);
  for (const n of shown) if (!seen.has(n.name)) visit(n.name);
  /** @type {Map<string, number>} */
  const layer = new Map(shown.map((n) => [n.name, 0]));
  for (let pass = 0; pass < shown.length; pass++) {
    for (const { from, to } of links.values()) {
      if (to === MODEL || back.has(from + "\u0000" + to)) continue;
      layer.set(to, Math.max(layer.get(to) ?? 0, (layer.get(from) ?? 0) + 1));
    }
  }
  // How many sandboxes each role had, and how many of them run now: started, and not gone.
  /** @type {Map<string, number>} */
  const had = new Map();
  /** @type {Map<string, number>} */
  const running = new Map();
  for (const lane of data.lanes || []) {
    const role = laneRole(lane, data);
    if (!role || lane.service) continue;
    had.set(role, (had.get(role) ?? 0) + 1);
    if (lane.sandbox.started !== null && lane.sandbox.gone === null) running.set(role, (running.get(role) ?? 0) + 1);
  }
  const [boxH, gapX, gapY, bow, stack] = [34, 64, 18, 28, 6];
  const chipOf = (/** @type {string} */ name) => ((had.get(name) ?? 0) > 1 ? "×" + had.get(name) : "");
  const placeOf = (/** @type {Role} */ n) => {
    const now = n.kind === "agent" ? 0 : (running.get(n.name) ?? 0);
    return (n.place || "-") + (now ? " · " + now + " running" : "");
  };
  const widthOf = (/** @type {Role} */ n) => {
    const chip = chipOf(n.name) ? (chipOf(n.name).length * 6.5 + 18) : 0;
    return Math.ceil(Math.max(n.name.length * NAME_CHAR_PX + (n.final_target ? 14 : 0) + chip, placeOf(n).length * PLACE_CHAR_PX)) + 14;
  };
  // Rows: the first layer in the IR's order, the model under the agent; each later layer at the
  // mean row of the roles that reach it, then by kind and the IR's order, a row to each.
  const layers = Math.max(0, ...layer.values()) + 1;
  /** @type {Map<string, number>} */
  const row = new Map();
  for (let at = 0; at < layers; at++) {
    const here = shown.filter((n) => layer.get(n.name) === at);
    const wanted = (/** @type {string} */ name) => {
      if (at === 0) return name === MODEL ? 0.5 : byName.get(name)?.index ?? 0;
      const from = [...links.values()].filter((l) => l.to === name && (layer.get(l.from) ?? 0) < at).map((l) => row.get(l.from) ?? 0);
      return from.length ? from.reduce((a, b) => a + b, 0) / from.length : Infinity;
    };
    here.sort((a, b) => wanted(a.name) - wanted(b.name) || (order[a.kind ?? ""] ?? 3) - (order[b.kind ?? ""] ?? 3) || a.index - b.index);
    let next = 0;
    for (const n of here) {
      const want = wanted(n.name);
      const r = Math.max(next, at === 0 || !Number.isFinite(want) ? next : Math.round(want));
      row.set(n.name, r);
      next = r + 1;
    }
  }
  const layerW = [...Array(layers)].map((_, at) => Math.max(0, ...shown.filter((n) => layer.get(n.name) === at).map(widthOf)));
  const xs = layerW.map((_, at) => bow + layerW.slice(0, at).reduce((sum, w) => sum + w + gapX, 0));
  /** @typedef {Role & { index: number, x: number, y: number, w: number, layer: number, row: number }} Box */
  /** @type {Map<string, Box>} */
  const nodes = new Map();
  for (const n of shown) {
    const [at, r] = [layer.get(n.name) ?? 0, row.get(n.name) ?? 0];
    nodes.set(n.name, { ...n, x: xs[at], y: 4 + r * (boxH + gapY), w: layerW[at], layer: at, row: r });
  }
  const width = xs[layers - 1] + layerW[layers - 1] + bow;
  const height = Math.max(0, ...[...nodes.values()].map((n) => n.y + boxH)) + stack + 4;
  const touches = (/** @type {string} */ from, /** @type {string} */ to) => !lit || (lit.focus ? from === lit.focus || to === lit.focus : lit.roles.has(from) && lit.roles.has(to));
  const touch = (/** @type {string} */ a, /** @type {string} */ b) => pairs.has(a + "\u0000" + b) || pairs.has(b + "\u0000" + a);
  // Each box's lines leave its right side and enter its left side spread out, in the order of
  // the boxes at their other ends, rather than from one point.
  const forward = [...links.values()].filter(({ from, to }) => (nodes.get(from)?.layer ?? 0) < (nodes.get(to)?.layer ?? 0));
  const yOf = (/** @type {string} */ name) => nodes.get(name)?.y ?? 0;
  const portY = (/** @type {string} */ name, /** @type {string} */ other, /** @type {"from" | "to"} */ side) => {
    const ends = forward.filter((l) => l[side] === name).map((l) => (side === "from" ? l.to : l.from));
    ends.sort((p, q) => yOf(p) - yOf(q));
    return yOf(name) + (boxH * (ends.indexOf(other) + 1)) / (ends.length + 1);
  };
  // The middle of each line drawn, by its two ends, either way round.
  /** @type {Map<string, [number, number]>} */
  const middles = new Map();
  const lines = [...links.values()].map(({ from, to }) => {
    const [a, b] = [nodes.get(from), nodes.get(to)];
    if (!a || !b) return null;
    const middle = (/** @type {number} */ x, /** @type {number} */ y) => (middles.set(from + "\u0000" + to, [x, y]), middles.set(to + "\u0000" + from, [x, y]));
    const count = between(from, to).reduce((sum, edge) => sum + edge.count, 0);
    const look = count
      ? { "stroke-width": Math.min(6, 1 + Math.log2(count)), class: touches(from, to) ? "" : "dim" }
      : { "stroke-width": 1.25, class: "idle" + (touches(from, to) ? "" : " dim") };
    if (a.layer < b.layer) {
      const [x1, y1, x2, y2] = [a.x + a.w, portY(from, to, "from"), b.x, portY(to, from, "to")];
      middle((x1 + x2) / 2, (y1 + y2) / 2);
      if (Math.abs(y1 - y2) < 0.5) return svg("line", { x1, y1, x2, y2, ...look });
      const mid = (x2 - x1) / 2;
      return svg("path", { d: "M" + x1 + " " + y1 + " C" + (x1 + mid) + " " + y1 + " " + (x2 - mid) + " " + y2 + " " + x2 + " " + y2, ...look });
    }
    if (a.layer > b.layer) {
      // A line back, of a cycle or to the model: from the left side to the right, level.
      const [x1, y1, x2, y2] = [a.x, a.y + boxH / 2, b.x + b.w, b.y + boxH / 2];
      middle((x1 + x2) / 2, (y1 + y2) / 2);
      const mid = (x1 - x2) / 2;
      return svg("path", { d: "M" + x1 + " " + y1 + " C" + (x1 - mid) + " " + y1 + " " + (x2 + mid) + " " + y2 + " " + x2 + " " + y2, ...look });
    }
    const [top, bottom] = a.row < b.row ? [a, b] : [b, a];
    const cx = top.x + Math.min(top.w, bottom.w) / 2;
    middle(cx, (top.y + boxH + bottom.y) / 2);
    if (bottom.row - top.row === 1) return svg("line", { x1: cx, y1: top.y + boxH, x2: cx, y2: bottom.y, ...look });
    const [side, outward] = a.layer === 0 ? [a.x, -bow] : [a.x + a.w, bow];
    const [y1, y2] = [top.y + boxH / 2, bottom.y + boxH / 2];
    // The middle of the curve, which bows three quarters of the way to its control points.
    middle(side + 0.75 * outward, (y1 + y2) / 2);
    return svg("path", { d: "M" + side + " " + y1 + " C" + (side + outward) + " " + y1 + " " + (side + outward) + " " + y2 + " " + side + " " + y2, ...look });
  });
  const boxes = [...nodes.values()].map((n) => {
    const many = (had.get(n.name) ?? 0) > 1;
    const none = n.kind !== "model" && !had.has(n.name) && data.lanes !== null;
    const chip = chipOf(n.name);
    const dim = lit && !lit.roles.has(n.name) && !(lit.focus && touch(lit.focus, n.name));
    const box = svg(
      "g",
      { class: ["node", n.kind, n.name === picked ? "on" : "", none ? "idle" : "", dim ? "dim" : ""].join(" "), transform: "translate(" + n.x + "," + n.y + ")", onclick: () => pick(n.name) },
      many ? svg("rect", { class: "stack", x: stack, y: stack, width: n.w, height: boxH, rx: 2 }) : null,
      many ? svg("rect", { class: "stack", x: stack / 2, y: stack / 2, width: n.w, height: boxH, rx: 2 }) : null,
      svg("rect", { width: n.w, height: boxH, rx: 2 }),
      n.final_target ? svg("rect", { class: "final", x: 6, y: 6, width: 8, height: 8, rx: 2 }) : null,
      svg("text", { x: n.final_target ? 20 : 6, y: 14 }, n.name),
      svg("text", { x: 6, y: 27, class: "place" }, placeOf(n)),
      chip ? svg("rect", { class: "chip", x: n.w - chip.length * 6.5 - 14, y: 4, width: chip.length * 6.5 + 8, height: 14, rx: 2 }) : null,
      chip ? svg("text", { class: "chip", x: n.w - chip.length * 3.25 - 10, y: 14.5, "text-anchor": "middle" }, chip) : null,
    );
    // The role's card: where it is, and what passed on each line that joins it.
    return card(box, () =>
      cardRows([
        ["Role", n.name + (n.final_target ? " (the final call's target)" : "")],
        ["On", n.place || null],
        ["Sandboxes", many ? String(had.get(n.name)) : none ? "None yet" : null],
        .../** @type {Array<[string, string]>} */ ([...pairs.keys()].map((key) => key.split("\u0000")).filter(([from, to]) => from === n.name || to === n.name).map(([from, to]) => [from + " → " + to, said(from, to)])),
      ]),
    );
  });
  const on = failed && failed.mark.between ? middles.get(failed.mark.between.join("\u0000")) : undefined;
  const box = failed && failed.mark.role ? nodes.get(failed.mark.role) : undefined;
  // On a box, the mark sits on its top right corner, clear of its name.
  const mark = failed && on ? talkMark(failed, on[0], on[1]) : failed && box ? talkMark(failed, box.x + box.w, box.y + 1) : null;
  fill(
    node,
    svg(
      "svg",
      {
        viewBox: "0 0 " + width + " " + height,
        width,
        height,
        // A click off every box puts down what is lit, as Esc does.
        onclick: (/** @type {MouseEvent} */ event) => {
          if (lit && !(event.target instanceof Element && event.target.closest(".node"))) pick(null);
        },
      },
      lines,
      boxes,
      mark,
    ),
    pairs.size ? null : h("p", { class: "empty" }, "The log shows no op or model request between roles yet."),
  );
}
