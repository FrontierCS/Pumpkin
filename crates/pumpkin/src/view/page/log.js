// A trace's Log tab: the records of the run's log that concern the caller's lane, its model
// requests and its ops among them, as the run's page lists them, with its filters of kind and
// text. The address keeps the filters (`lkind=`, `lq=`) and the record selected (`lsel=<byte>`).
// A record of one of the caller's model requests selects that request's step in the Steps tab
// (`go`); another shows itself beside the list. The log is read to its end each time the tab is
// shown.

import { bytes, fill, get, h, plural, route, setQuery } from "./h.js";
import { drawKinds, folded, holds, lanesOf, pastWindow, recordRow } from "./records.js";
import { RowList } from "./rows.js";

/**
 * @typedef {import("./records.js").LogRecord} LogRecord
 * @typedef {import("./records.js").Window} Window
 * @typedef {import("./records.js").Item} Item
 */

/** @param {{ base: string, caller: string, started: () => number | null, go: (key: string) => void }} ctx */
export function logView(ctx) {
  const lane = ctx.caller.replace("-", " ");
  const { query } = route();
  /** @type {{ records: LogRecord[], cursor: number, epoch: number | null, open: Set<number>, kinds: string, shown: LogRecord[] }} */
  const state = { records: [], cursor: 0, epoch: null, open: new Set(), kinds: "", shown: [] };
  const kinds = /** @type {HTMLSelectElement} */ (h("select", { title: "Show the records of one kind" }));
  const search = /** @type {HTMLInputElement} */ (h("input", { type: "search", class: "search", placeholder: "Filter records", title: "Keep the records that hold this text", value: query.get("lq") || "" }));
  const counted = h("span", { class: "counted" });
  const record = h("div", { class: "inspector panel" });
  // The step that a record of one of the caller's model requests is of.
  const stepOf = (/** @type {LogRecord} */ r) => {
    const event = r.event;
    return event && event.caller === ctx.caller && (event.kind === "model_request_started" || event.kind === "model_answered") ? Number(event.request) : null;
  };
  /** @type {RowList<Item>} */
  const list = new RowList(
    (item) => {
      const row = recordRow(item, state.open, ctx.started());
      if (!("fold" in item) && stepOf(item) !== null) row.classList.add("of-step");
      return row;
    },
    (item) => {
      if ("fold" in item) {
        const first = item.fold[0].byte;
        if (!state.open.delete(first)) state.open.add(first);
        return draw();
      }
      choose(item);
    },
  );
  const node = h("div", { class: "tlog" }, h("div", { class: "filter" }, kinds, search, counted), h("div", { class: "log-split" }, list.node, record));

  const choose = (/** @type {LogRecord} */ r) => {
    setQuery({ lsel: r.byte });
    const step = stepOf(r);
    if (step !== null) return ctx.go(String(step));
    draw();
  };

  const draw = () => {
    const { query } = route();
    const kind = query.get("lkind");
    const needle = search.value.trim().toLowerCase();
    const shown = state.records.filter((r) => holds(r, kind, needle));
    drawKinds(kinds, state, kind);
    const selected = query.get("lsel");
    const holder = shown.find((r) => String(r.byte) === selected);
    if (holder) {
      for (const item of folded(shown, new Set())) if ("fold" in item && item.fold.includes(holder)) state.open.add(item.fold[0].byte);
    }
    state.shown = shown;
    const items = folded(shown, state.open);
    list.set(items, holder ? items.indexOf(holder) : -1);
    if (holder) list.select(list.selected, true);
    counted.textContent = shown.length.toLocaleString("en-US") + " of " + plural(state.records.length, "record") + " of " + lane;
    fill(
      record,
      holder
        ? [h("h3", {}, h("span", { class: "kind" }, "Record"), h("span", { class: "id dim" }, "At byte " + holder.byte)), h("pre", { class: "sent" }, JSON.stringify(holder, null, 2))]
        : h("p", { class: "empty" }, "Select a record (j and k step). A record of a model request selects its step in the Steps tab."),
    );
  };
  kinds.addEventListener("change", () => (setQuery({ lkind: kinds.value || null }), draw()));
  search.addEventListener("input", () => (setQuery({ lq: search.value.trim().toLowerCase() || null }), draw()));

  // Reads the log from where the last read ended to its end now, keeping the caller's records.
  const read = async () => {
    try {
      const run = await get(ctx.base);
      if (state.epoch !== run.log_epoch) Object.assign(state, { records: [], cursor: 0, epoch: run.log_epoch });
      while (state.cursor < run.end) {
        counted.textContent = "Reading the log: " + bytes(state.cursor) + " of " + bytes(run.end);
        /** @type {Window} */
        const window = await get(ctx.base + "/log?at=" + state.cursor + "&log_epoch=" + run.log_epoch);
        state.records.push(...pastWindow(window, state).filter((r) => lanesOf(r).has(lane)));
      }
    } catch (error) {
      counted.textContent = "Reading the log failed: " + (error instanceof Error ? error.message : String(error));
      return;
    }
    draw();
  };

  return {
    node,
    show: read,
    // j and k select the next and previous record shown, past folds closed.
    key: (/** @type {KeyboardEvent} */ event) => {
      const by = { j: 1, k: -1 }[event.key];
      if (!by) return;
      event.preventDefault();
      for (let at = list.selected + by; at >= 0 && at < list.items.length; at += by) {
        const item = list.items[at];
        if (!("fold" in item)) return (setQuery({ lsel: item.byte }), draw());
      }
    },
  };
}
