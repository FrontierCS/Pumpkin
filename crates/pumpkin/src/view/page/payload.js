// A payload that an op sent, as the record stores it, on a page of its own, which a record's
// inspector links to: shown file by file if it
// is a tar, as most are, else whole. A patch is drawn as a diff, file by file, hunk by hunk, with
// each line's numbers in the old file and the new, and the part that changed of a line changed in
// place marked, as the FrontierCS site draws one; JSON indented, coloured as the site colours code. Any other text is shown as it is, binary bytes by their size alone.

import { Refused, bytes, fill, getBytes, h, show } from "./h.js";

/**
 * @typedef {{ name: string, data: Uint8Array }} File a file of a payload: its path, and its bytes
 * @typedef {{ kind: string, n: number | null, text: string, changed?: [number, number] }} DiffRow a line of a diff: its number, in the old file if removed, else in the new, and the part of it that changed, if it changed in place
 * @typedef {{ path: string, kind: string, rows: DiffRow[], added: number, removed: number }} DiffFile a file of a diff
 * @typedef {import("./h.js").Child} Child
 */

// The most of a file's text that is drawn; the rest is cut, with a note.
const SHOWN_BYTES = 512 * 1024;

// The payload `{ digest, size }` that a log record names, of an op that sent one or a judge call
// that took one; null if it names none.
/** @param {Record<string, any>} record @returns {{ digest: string, size: number } | null} */
export function payloadOf(record) {
  const named = (/** @type {any} */ value) => (value && typeof value.digest === "string" && typeof value.size === "number" ? { digest: value.digest, size: value.size } : null);
  const event = record.event || {};
  const found = named(event.request && event.request.payload) || named(event.payload);
  if (found) return found;
  for (const action of record.actions || []) {
    const payload = named(action.payload) || named(action.reply && action.reply.payload);
    if (payload) return payload;
  }
  return null;
}

// A link to the payload `{ digest, size }` of run `id`, which opens its page in a new tab.
/** @param {string} id @param {{ digest: string, size: number }} payload */
export function payloadLink(id, payload) {
  const hex = payload.digest.replace(/^sha256:/, "");
  return h(
    "p",
    { class: "tools" },
    h("span", { class: "name" }, "Payload"),
    h("span", { class: "num", title: payload.digest }, bytes(payload.size)),
    h("a", { href: "#/run/" + encodeURIComponent(id) + "/payload/" + hex, target: "_blank", rel: "noopener", title: "Open the payload in a new tab" }, "Open"),
  );
}

// The page of the payload `hex` of run `id`: its files, read from the record.
/** @param {string} id @param {string} hex */
export async function payloadView(id, hex) {
  const page = h("main", { class: "payload-page" }, h("p", { class: "dim" }, "Reading…"));
  show(page);
  try {
    fill(page, h("h2", {}, "Payload ", h("span", { class: "num mono" }, hex.slice(0, 12))), drawFiles(filesOf(await getBytes("/api/runs/" + encodeURIComponent(id) + "/payload/" + hex))));
  } catch (error) {
    const why = error instanceof Refused || error instanceof Error ? error.message : String(error);
    fill(page, h("p", { class: "error" }, "Reading the payload failed: " + why));
  }
}

// The files of `data`: each regular file of a tar, in its order, or `data` whole as `payload` if
// it is no tar.
/** @param {Uint8Array} data @returns {File[]} */
export function filesOf(data) {
  const ascii = (/** @type {number} */ from, /** @type {number} */ to) => {
    const end = data.subarray(from, to).indexOf(0);
    return new TextDecoder().decode(data.subarray(from, end < 0 ? to : from + end));
  };
  if (data.length < 512 || ascii(257, 262) !== "ustar") return [{ name: "payload", data }];
  /** @type {File[]} */
  const files = [];
  let long = "";
  for (let at = 0; at + 512 <= data.length; ) {
    if (data.subarray(at, at + 512).every((byte) => byte === 0)) break;
    const size = parseInt(ascii(at + 124, at + 136).trim() || "0", 8);
    const type = String.fromCharCode(data[at + 156]);
    const prefix = ascii(at + 345, at + 500);
    const name = long || (prefix ? prefix + "/" : "") + ascii(at, at + 100);
    const body = data.subarray(at + 512, at + 512 + size);
    long = "";
    if (type === "x") {
      // A pax header: its `path` names the next file.
      const path = new TextDecoder().decode(body).match(/^\d+ path=(.*)$/m);
      if (path) long = path[1];
    } else if (type === "L") {
      // A GNU long name: the next file's.
      long = ascii(at + 512, at + 512 + size);
    } else if (type === "0" || type === "\0") {
      files.push({ name, data: body });
    }
    at += 512 + Math.ceil(size / 512) * 512;
  }
  return files;
}

// `files`, each under its path and size: a patch as a diff, JSON as a tree, other text as it is,
// open; binary bytes by their size.
/** @param {File[]} files */
function drawFiles(files) {
  if (!files.length) return h("p", { class: "empty" }, "The payload is a tar that holds no file.");
  return files.map((file) => {
    /** @type {string | null} */
    let text = null;
    try {
      text = new TextDecoder("utf-8", { fatal: true }).decode(file.data.subarray(0, SHOWN_BYTES));
    } catch {
      text = null;
    }
    const cut = file.data.length > SHOWN_BYTES ? h("p", { class: "dim" }, "The first " + bytes(SHOWN_BYTES) + " of " + bytes(file.data.length) + " shown.") : null;
    /** @type {Child} */
    let body = h("p", { class: "dim" }, "Binary, " + bytes(file.data.length) + ".");
    if (text !== null && (/\.(patch|diff)$/.test(file.name) || /^diff --git /m.test(text))) body = drawDiff(text);
    else if (text !== null && /\.json$/.test(file.name)) body = drawJsonText(text);
    else if (text !== null) body = h("pre", { class: "sent" }, text);
    return h("section", { class: "payload-file" }, h("h3", {}, h("span", { class: "mono" }, file.name), " ", h("span", { class: "num" }, bytes(file.data.length))), body, cut);
  });
}

// A unified diff, as the patch viewer of the FrontierCS site draws one, after Claude Code's design:
// each file as `Update(path)`, or `Write(path)` for a new one and `Delete(path)` for a removed one,
// over an elbow line of how many lines it adds and removes; then its lines, each numbered, the old
// line's number on a removal and the new one's otherwise, `…` between hunks. Within a removed line
// and the added line that replaces it, the words that changed are marked, unless most of the line
// changed. A file folds, open as first drawn: its page has the room.
/** @param {string} text */
export function drawDiff(text) {
  /** @type {DiffFile[]} */
  const files = [];
  /** @type {DiffFile | null} */
  let file = null;
  let [old, now, inHunk] = [0, 0, false];
  for (const line of text.split("\n")) {
    if (line.startsWith("diff --git ")) {
      file = { path: line.replace(/^diff --git a\/.*? b\//, ""), kind: "Update", rows: [], added: 0, removed: 0 };
      files.push(file);
      inHunk = false;
      continue;
    }
    if (!file) {
      // A unified diff without git's header: its file is named by `+++`.
      const named = line.match(/^\+\+\+ (?:b\/)?(.+)$/);
      if (!named) continue;
      file = { path: named[1], kind: "Update", rows: [], added: 0, removed: 0 };
      files.push(file);
      continue;
    }
    const hunk = line.match(/^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/);
    if (hunk) {
      if (file.rows.length) file.rows.push({ kind: "gap", n: null, text: "" });
      [old, now, inHunk] = [Number(hunk[1]), Number(hunk[2]), true];
    } else if (!inHunk) {
      if (line.startsWith("new file mode") || line === "--- /dev/null") file.kind = "Write";
      else if (line.startsWith("deleted file mode") || line === "+++ /dev/null") file.kind = "Delete";
    } else if (line.startsWith("+")) {
      file.rows.push({ kind: "add", n: now++, text: line.slice(1) });
      file.added += 1;
    } else if (line.startsWith("-")) {
      file.rows.push({ kind: "del", n: old++, text: line.slice(1) });
      file.removed += 1;
    } else if (line.startsWith(" ")) {
      file.rows.push({ kind: "ctx", n: now++, text: line.slice(1) });
      old += 1;
    }
  }
  for (const f of files) markChanges(f.rows);
  /** @param {number} n @returns {Child[]} */
  const lines = (n) => [h("b", {}, String(n)), n === 1 ? " line" : " lines"];
  const sign = { add: "+", del: "-", ctx: "", gap: "" };
  return h(
    "div",
    { class: "cc" },
    files.map((f) => {
      /** @type {Child[]} */
      const sum =
        f.kind === "Write"
          ? ["Wrote ", ...lines(f.added)]
          : f.kind === "Delete"
            ? ["Removed ", ...lines(f.removed)]
            : [...(f.added ? ["Added ", ...lines(f.added)] : []), ...(f.removed ? [f.added ? ", removed " : "Removed ", ...lines(f.removed)] : [])];
      const width = String(Math.max(1, ...f.rows.map((row) => row.n ?? 0))).length + 2;
      const rows = f.rows.map((row) =>
        h(
          "div",
          { class: "r " + row.kind },
          h("span", { class: "n" }, row.n === null ? "" : String(row.n)),
          h("span", { class: "s" }, /** @type {Record<string, string>} */ (sign)[row.kind]),
          h("span", { class: "c" }, row.kind === "gap" ? "…" : changedParts(row)),
        ),
      );
      return h(
        "details",
        { class: "ccf", open: true },
        h("summary", {}, h("span", { class: "cct" }, h("b", {}, f.kind), "(" + f.path + ")"), h("span", { class: "ccs" }, sum)),
        h("div", { class: "ccd", style: { "--nw": width + "ch" } }, rows),
      );
    }),
  );
}

// One token of a line, for marking the words that changed: a run of spaces, a word, or one other
// character.
const TOKEN = /\s+|\w+|[^\s\w]/g;

// Marks the words that changed of each line changed in place: in each run of removed lines then
// added lines, the n-th removed and the n-th added, their tokens but the start and the end they
// share; nothing if they share less than most of their text.
/** @param {DiffRow[]} rows */
function markChanges(rows) {
  for (let at = 0; at < rows.length; ) {
    let dels = at;
    while (dels < rows.length && rows[dels].kind === "del") dels++;
    let adds = dels;
    while (adds < rows.length && rows[adds].kind === "add") adds++;
    for (let k = 0; k < Math.min(dels - at, adds - dels); k++) {
      const [del, add] = [rows[at + k], rows[dels + k]];
      const [x, y] = [del.text.match(TOKEN) || [], add.text.match(TOKEN) || []];
      let start = 0;
      while (start < x.length && start < y.length && x[start] === y[start]) start++;
      let end = 0;
      while (end < x.length - start && end < y.length - start && x[x.length - 1 - end] === y[y.length - 1 - end]) end++;
      const [mx, my] = [x.slice(start, x.length - end).join(""), y.slice(start, y.length - end).join("")];
      // A line mostly rewritten marks nothing, as Claude Code does.
      if (mx.length + my.length > 0.4 * (del.text.length + add.text.length)) continue;
      const from = (/** @type {string[]} */ t) => t.slice(0, start).join("").length;
      del.changed = [from(x), from(x) + mx.length];
      add.changed = [from(y), from(y) + my.length];
    }
    at = Math.max(adds, at + 1);
  }
}

// `row`'s text, the words that changed, if any, marked.
/** @param {DiffRow} row @returns {Child[]} */
function changedParts(row) {
  if (!row.changed) return [row.text];
  const [from, to] = row.changed;
  return [row.text.slice(0, from), h("mark", {}, row.text.slice(from, to)), row.text.slice(to)];
}

// `text` as indented JSON if it parses, else as it is.
/** @param {string} text */
function drawJsonText(text) {
  try {
    return jsonText(JSON.parse(text));
  } catch {
    return h("pre", { class: "sent" }, text);
  }
}

// One JSON token: a string, a key if a colon follows it, a number, or `true`, `false` or `null`.
const JSON_TOKEN = /("(?:\\.|[^"\\])*")(\s*:)?|\b(?:true|false|null)\b|-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?/g;

// `value` as JSON indented by 2, as `JSON.stringify` writes it, each key, string and other value
// coloured by its kind.
/** @param {any} value */
export function jsonText(value) {
  const text = JSON.stringify(value, null, 2) ?? String(value);
  /** @type {Child[]} */
  const parts = [];
  let at = 0;
  for (const token of text.matchAll(JSON_TOKEN)) {
    const index = token.index ?? 0;
    parts.push(text.slice(at, index));
    const [whole, string, colon] = token;
    if (string && colon) parts.push(h("span", { class: "j-key" }, string), colon);
    else parts.push(h("span", { class: string ? "j-string" : /^[tfn]/.test(whole) ? "j-literal" : "j-number" }, whole));
    at = index + whole.length;
  }
  parts.push(text.slice(at));
  return h("pre", { class: "jsonview" }, parts);
}
