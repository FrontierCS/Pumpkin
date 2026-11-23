// A failure, as the run page draws every one, a run's ending or a call's, from one object: whose
// fault the record says it is, its subject as a link that selects it, when it failed, what the
// subject left, and the reason, as the server cuts strings; and the one mark that the lanes and
// the diagram carry on the subject. Nothing here reads a reason's words, or asks what kind a subject is.

import { bytes, clock, fill, getText, h, plural, sentence, svg, utc } from "./h.js";

/**
 * @typedef {object} Failure a failure that the record states, or a run's ending without a result
 * @property {string} state how it ended, as words: `failed`, `finished without a result`
 * @property {string | null} reason the record's reason, as the server cuts strings
 * @property {string} [fault] whose fault the record says it is
 * @property {number | null} at seconds since the run started
 * @property {{ kind: string, [id: string]: any }} subject what failed, by the ids its record names it by
 * @property {Array<{ name: string, path: string, bytes: number }>} evidence what the record keeps of the subject, each piece at `path` below the run's address
 * @property {Mark} mark
 * @typedef {object} Mark where a failure is marked
 * @property {string | null} lane the subject's lane; none for the run
 * @property {number | null} at
 * @property {number | null} record the byte of the log's record that states it
 * @property {string | null} role
 * @property {[string, string] | null} between the roles at the subject's two ends
 * @typedef {object} Context
 * @property {string} base the run's address in the server's API
 * @property {number | null | undefined} started the run's start, a Unix time in seconds
 * @property {(failure: Failure) => void} pick selects a failure's subject
 */

// The longest reason, in characters and in lines, whose fold starts open.
const REASON_OPEN_CHARS = 320;
const REASON_OPEN_LINES = 3;
// How much of a piece that the subject left is shown.
const LEFT_SHOWN = 256 * 1024;

// A failure's subject in a line: its kind, and its lane if it has one.
/** @param {Failure} failure */
const subjectOf = (failure) => failure.subject.kind + (failure.mark.lane ? " · " + failure.mark.lane : "");

// What a failure's mark says on hover: what failed, and when.
/** @param {Failure} failure */
const titleOf = (failure) => sentence(subjectOf(failure)) + ", " + clock(failure.at);

// The address that selects a failure's subject, for a failure that a record of the log states.
/** @param {string} id @param {Failure} failure */
const hrefOf = (id, failure) => "#/run/" + encodeURIComponent(id) + "?sel=" + failure.mark.record;

// A piece that the subject left, folded under its name and size, read from the record when the
// fold first opens, and shown as kept.
/** @param {Failure["evidence"][number]} piece @param {string} base */
function left(piece, base) {
  const shown = h("pre", { class: "kept" });
  const fold = /** @type {HTMLDetailsElement} */ (h("details", {}, h("summary", {}, "What the subject left", h("span", { class: "hint" }, piece.name + " · " + bytes(piece.bytes))), shown));
  let read = false;
  fold.addEventListener("toggle", async () => {
    if (!fold.open || read) return;
    read = true;
    shown.textContent = "Reading…";
    try {
      const text = await getText(base + "/" + piece.path);
      shown.textContent = text.length > LEFT_SHOWN ? text.slice(0, LEFT_SHOWN) : text;
      if (text.length > LEFT_SHOWN) fold.append(h("p", { class: "hint" }, "The first " + bytes(LEFT_SHOWN) + " shown"));
    } catch (error) {
      read = false;
      shown.textContent = "Reading it failed: " + (error instanceof Error ? error.message : String(error));
    }
  });
  return fold;
}

// The parts of a failure below its state: the fault, the subject and the time, each under its
// name; what the subject left; the reason.
/** @param {string} id @param {Failure} failure @param {Context} ctx @returns {HTMLElement[]} */
export function failureParts(id, failure, ctx) {
  const { subject, mark, reason } = failure;
  // The subject's ids, but one that only says its lane's name again.
  const ids = Object.entries(subject)
    .filter(([name, value]) => name !== "kind" && value !== null)
    .map(([name, value]) => name + " " + value)
    .filter((said) => said !== mark.lane);
  const select = h("a", { href: hrefOf(id, failure), title: "Select it in the lanes and the diagram", onclick: (/** @type {Event} */ event) => (event.preventDefault(), ctx.pick(failure)) }, subjectOf(failure));
  const kv = (/** @type {string} */ name, /** @type {string} */ kind, /** @type {import("./h.js").Child[]} */ ...value) => h("div", { class: "kv" }, h("span", { class: "k" }, name), h("span", { class: "v " + kind }, value));
  const lines = reason === null ? 0 : reason.split("\n").length;
  const open = reason !== null && reason.length <= REASON_OPEN_CHARS && lines <= REASON_OPEN_LINES;
  return [
    h(
      "div",
      { class: "values" },
      failure.fault ? kv("Fault", "", sentence(failure.fault)) : kv("Fault", "none", "Not stated"),
      kv("Subject", "num", mark.record === null ? subjectOf(failure) : select, ids.length ? h("small", {}, ids.join(" · ")) : null),
      failure.at === null ? kv("At", "none", "Not recorded") : kv("At", "num", h("span", { title: utc(ctx.started, failure.at) }, clock(failure.at))),
    ),
    h(
      "div",
      { class: "folds" },
      failure.evidence.length ? failure.evidence.map((piece) => left(piece, ctx.base)) : h("div", { class: "none" }, "What the subject left", h("span", { class: "hint" }, "nothing kept")),
      reason === null
        ? h("div", { class: "none" }, "Reason", h("span", { class: "hint" }, "none recorded"))
        : h("details", { open }, h("summary", {}, "Reason, as the record gives it", lines > 1 ? h("span", { class: "hint" }, plural(lines, "line")) : null), h("p", { class: "reason" + (lines > 1 ? " mono" : "") }, reason)),
    ),
  ];
}

// A failure whole, a size smaller, as the inspector shows one of a call, an agent or a lease.
/** @param {string} id @param {Failure} failure @param {Context} ctx */
export function failureBox(id, failure, ctx) {
  return h("div", { class: "failure small" }, h("div", { class: "verdict" }, h("span", { class: "mark failed" }), h("span", {}, sentence(failure.state))), failureParts(id, failure, ctx));
}

// The mark of `failure` on a lane's track or on the time axis, at `left`: a button that selects
// the record that states the failure, if one does.
/** @param {Failure} failure @param {string} left @param {(failure: Failure) => void} pick */
export function laneMark(failure, left, pick) {
  if (failure.mark.record === null) return h("div", { class: "failmark", style: { left }, title: titleOf(failure) });
  return h("button", { class: "failmark", style: { left }, title: titleOf(failure) + ": select its record", onclick: () => pick(failure) });
}

// The mark of `failure` in the diagram, its middle at `x`, `y`.
/** @param {Failure} failure @param {number} x @param {number} y */
export function talkMark(failure, x, y) {
  return svg("rect", { class: "failmark", x: x - 5, y: y - 5, width: 10, height: 10, rx: 2 }, svg("title", {}, titleOf(failure)));
}
