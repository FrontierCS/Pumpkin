// The page of `pumpkin view`: the runs ([`list.js`]), a run ([`run.js`]) and a caller's trace
// ([`trace.js`]), each drawn from the JSON that the server reads from the records, as the address
// says: `#/`, `#/run/<id>`, `#/run/<id>/trace?caller=<caller>` or `#/run/<id>/payload/<hex>`, with
// what is selected in its query.

import { byId, fill, h, key, route, setKeys } from "./h.js";
import { keyOf } from "./key.js";
import { runsView } from "./list.js";
import { payloadView } from "./payload.js";
import { runView } from "./run.js";
import { traceView } from "./trace.js";

function render() {
  const { parts } = route();
  setKeys(null);
  byId("follow").replaceChildren();
  byId("notice").hidden = true;
  const where = byId("where");
  if (parts[0] === "run" && parts[1] && parts[2] === "payload" && parts[3]) {
    const sep = () => h("span", { class: "sep" }, "›");
    fill(where, h("a", { href: "#/", title: "All runs" }, "Runs"), sep(), h("a", { href: "#/run/" + encodeURIComponent(parts[1]), class: "id", title: "The run" }, parts[1]), sep(), h("span", {}, "payload"));
    keyOf("payload");
    payloadView(parts[1], parts[3]);
  } else if (parts[0] === "run" && parts[1] && parts[2] === "trace") {
    const caller = route().query.get("caller") || "agent";
    const sep = () => h("span", { class: "sep" }, "›");
    fill(where, h("a", { href: "#/", title: "All runs" }, "Runs"), sep(), h("a", { href: "#/run/" + encodeURIComponent(parts[1]), class: "id", title: "The run" }, parts[1]), sep(), h("span", { class: "id" }, caller.replace("-", " ")));
    keyOf("trace");
    traceView(parts[1]);
  } else if (parts[0] === "run" && parts[1]) {
    fill(where, h("a", { href: "#/", title: "All runs" }, "Runs"), h("span", { class: "sep" }, "›"), h("span", { class: "id" }, parts[1]));
    keyOf("run");
    runView(parts[1]);
  } else {
    fill(where, h("span", {}, "Runs"));
    keyOf("list");
    runsView();
  }
}

addEventListener("hashchange", (event) => {
  // A change of the query alone is the view's own, which it has drawn, but for a trace's `caller`:
  // another caller is another trace.
  const path = (/** @type {string} */ url) => {
    const [at, query] = new URL(url).hash.split("?");
    return at + " " + (new URLSearchParams(query || "").get("caller") || "");
  };
  if (path(event.oldURL) !== path(event.newURL)) render();
});
addEventListener("keydown", key);
render();
