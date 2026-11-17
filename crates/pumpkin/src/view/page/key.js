// The colour key: what each colour of the page means, behind the top bar's `Colour key`, as
// Temporal's event history opens its key on demand
// (https://github.com/temporalio/ui/blob/adb1e2e50454e29addde5db8c53f2a1efeb5a5a6/src/lib/components/lines-and-dots/event-history-legend.svelte#L110-L138).
// `KEY` is the one place a colour's meaning is written, and each swatch is drawn in the token of
// `style.css` that the page draws that meaning with.

import { byId, fill, h } from "./h.js";

/** @typedef {"list" | "run" | "trace" | "payload"} Page */
/**
 * A colour's swatch: `look` is `box` for a mark's square, `bar` for a lane's bar, `tick` for a
 * line, `ink` for words in the colour; `alpha` is the share of it the page draws, if not whole.
 * @typedef {{ token: string, look: "box" | "bar" | "tick" | "ink", words: string, alpha?: number }} Swatch
 */

/** @type {Array<{ name: string, pages: Page[], swatches: Swatch[] }>} */
const KEY = [
  {
    name: "State of a run or a step",
    pages: ["list", "run", "trace"],
    swatches: [
      { token: "--live", look: "box", words: "Live, or in flight" },
      { token: "--ok-bar", look: "box", words: "Finished with a result, or answered" },
      { token: "--warn-bar", look: "box", words: "Finished without a result, refused or not forwarded" },
      { token: "--fail-bar", look: "box", words: "Failed, lost or interrupted" },
      { token: "--edge", look: "box", words: "Not known yet" },
    ],
  },
  {
    name: "Roles, in the lanes and the log",
    pages: ["run"],
    swatches: [
      { token: "--agent", look: "bar", words: "The agent" },
      { token: "--target", look: "bar", words: "A call's target" },
      { token: "--runner", look: "bar", words: "A runner, and a lease's work" },
      { token: "--model", look: "tick", words: "A model answer" },
      { token: "--fail-bar", look: "tick", words: "A failed model answer, or failed work" },
    ],
  },
  {
    name: "The model lane",
    pages: ["trace"],
    swatches: [
      { token: "--edge", look: "bar", words: "Waiting for the first byte", alpha: 0.7 },
      { token: "--model", look: "bar", words: "Waiting for the first token", alpha: 0.45 },
      { token: "--model", look: "bar", words: "Decoding" },
      { token: "--live", look: "bar", words: "In flight" },
      { token: "--fail-bar", look: "bar", words: "Failed" },
    ],
  },
  {
    name: "The tools and ops lanes",
    pages: ["trace"],
    swatches: [
      { token: "--runner", look: "bar", words: "Tools and the harness, between two steps", alpha: 0.55 },
      { token: "--runner", look: "bar", words: "A lease's op" },
      { token: "--target", look: "bar", words: "A call's op" },
      { token: "--agent", look: "bar", words: "A now function's call" },
      { token: "--fail-bar", look: "bar", words: "A failed op" },
    ],
  },
  {
    name: "Kinds of entry",
    pages: ["trace"],
    swatches: [
      { token: "--agent-ink", look: "ink", words: "user, of the agent" },
      { token: "--target-ink", look: "ink", words: "user, of a call" },
      { token: "--model-ink", look: "ink", words: "assistant and reasoning, the model's" },
      { token: "--runner-ink", look: "ink", words: "call and output, a tool's" },
      { token: "--fail", look: "ink", words: "unread" },
    ],
  },
  {
    name: "Marks",
    pages: ["list", "run", "trace"],
    swatches: [
      { token: "--select", look: "box", words: "Selected" },
      { token: "--mark-bar", look: "box", words: "A search's match" },
      { token: "--accent", look: "ink", words: "A link, the focus, or what is chosen" },
    ],
  },
];

const button = byId("colour-key-button");
const panel = byId("colour-key");

/** @param {Swatch} swatch */
function drawn({ token, look, words, alpha }) {
  const style = { [look === "ink" ? "color" : "background"]: "var(" + token + ")", opacity: alpha ?? 1 };
  return h("li", {}, h("span", { class: "swatch sw-" + look, style }, look === "ink" ? "Aa" : ""), words);
}

// Shows the key of `page`'s colours, open or not as it was.
/** @param {Page} page */
export function keyOf(page) {
  fill(
    panel,
    KEY.filter((group) => group.pages.includes(page)).map((group) => h("section", {}, h("h4", {}, group.name), h("ul", {}, group.swatches.map(drawn)))),
  );
}

/** @param {boolean} open */
function setOpen(open) {
  panel.hidden = !open;
  button.setAttribute("aria-expanded", String(open));
  if (open) panel.focus();
}

button.addEventListener("click", () => setOpen(button.getAttribute("aria-expanded") !== "true"));
// Esc closes the key before a view's keys see it, which would clear their selection.
addEventListener(
  "keydown",
  (event) => {
    if (event.key !== "Escape" || panel.hidden) return;
    event.stopPropagation();
    setOpen(false);
    button.focus();
  },
  true,
);
addEventListener("pointerdown", (event) => {
  if (!panel.hidden && event.target instanceof Node && !panel.contains(event.target) && !button.contains(event.target)) setOpen(false);
});
