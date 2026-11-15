// The page of `pumpkin view`: the runs ([`list.js`]) and a run ([`run.js`]), each drawn from the
// JSON that the server reads from the records, as the address says: `#/` or `#/run/<id>`, with
// what is selected in its query.

import { byId, fill, h, key, route, setKeys } from "./h.js";
import { runsView } from "./list.js";
import { runView } from "./run.js";

function render() {
  const { parts } = route();
  setKeys(null);
  byId("follow").replaceChildren();
  const where = byId("where");
  if (parts[0] === "run" && parts[1]) {
    fill(where, h("a", { href: "#/", title: "All runs" }, "Runs"), h("span", { class: "sep" }, "›"), h("span", { class: "id" }, parts[1]));
    runView(parts[1]);
  } else {
    fill(where, h("span", {}, "Runs"));
    runsView();
  }
}

addEventListener("hashchange", (event) => {
  // A change of the query alone is the view's own, which it has drawn.
  const path = (/** @type {string} */ url) => new URL(url).hash.split("?")[0];
  if (path(event.oldURL) !== path(event.newURL)) render();
});
addEventListener("keydown", key);
render();
