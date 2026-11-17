// A caller's steps as the server's `trace/<caller>/steps` reply gives them, in columns, and what
// the page says of a step from its row: its threads, its place in its thread, its times, its
// tokens, its calls, and whether it is a retry or a repeat. The server derives every fact; the
// page only puts them in words and units.

import { count, seconds } from "./h.js";

/**
 * @typedef {object} Columns `trace/<caller>/steps`' `steps`: equal-length arrays, in request order
 * @property {number[]} request
 * @property {Array<number | null>} thread the root request of the step's thread; null in flight
 * @property {Array<number | null>} parent
 * @property {Array<number | null>} turn
 * @property {Array<number | null>} retry_of
 * @property {Array<number | null>} retry_count which retry it is, from 1, along retries each of the one before
 * @property {Array<number | null>} repeat_of
 * @property {Array<string | null>} start_ns
 * @property {Array<string | null>} first_byte_ns
 * @property {Array<string | null>} first_token_ns
 * @property {Array<string | null>} end_ns
 * @property {Array<number | null>} model_ns its model time, from start to end, as the server measures it
 * @property {Array<string | null>} answer
 * @property {Array<number | null>} status
 * @property {Array<{ code?: string } | null>} failed
 * @property {Array<string | null>} upstream
 * @property {Array<string | null>} api
 * @property {Array<number | null>} input
 * @property {Array<number | null>} cached
 * @property {Array<number | null>} cache_hit the fraction of its input tokens cached
 * @property {Array<number | null>} cache_write
 * @property {Array<number | null>} output
 * @property {Array<number | null>} reasoning
 * @property {Array<number | null>} sent_bytes
 * @property {Array<number | null>} kept_bytes
 * @property {Array<number | null>} back_bytes
 * @property {Array<number | null>} items null in flight, or for a body the record kept whole
 * @property {Array<number | null>} added the items past its parent's list, echoes of its answer included; null as `items` is
 * @property {Array<number | null>} added_entries the entries `entries` gives of those it added
 * @property {Array<number | null>} answers its answer's entries; in flight, those so far
 * @property {string[][]} calls
 * @property {Array<number | null>} tools
 * @property {Array<string[] | null>} differs which of "system" (the system prompt's field) and "tools" differ from its parent's; null with no parent
 * @property {Array<string | null>} stop
 * @property {Array<boolean | null>} whole
 * @property {Array<string[] | null>} notes
 * @typedef {object} StepsReply
 * @property {string} caller
 * @property {number} threads
 * @property {{ thread: number[], span_ns: Array<number | null> }} thread_spans each thread of the rows sent, from its first start to its last end
 * @property {Columns} steps
 * @property {Array<{ digest: string | null, names: string[] }>} tool_lists
 * @property {number} end
 * @property {number} log_epoch
 * @property {boolean} live whether the run is live
 * @typedef {object} Entry an entry of `trace/<caller>/entries`
 * @property {string} at its address: `<request>/in/<item>/<part>`, `<request>/head/<field>/<part>`, `<request>/out/<part>`
 * @property {number} request
 * @property {number} index
 * @property {"added" | "answer"} side
 * @property {string} kind
 * @property {string} text
 * @property {string | null} digest
 * @property {string} [call_id]
 * @property {string} [name]
 * @property {string} [call] the address of the call an output answers
 * @property {string} [output] the address of a call's output
 * @property {boolean} [error] of an output: its tool reported a failure
 * @property {number} [text_bytes] of `trace/<caller>/entry`: the whole text's length, in UTF-8 bytes
 * @typedef {object} StepStats `exchange/<n>`'s `stats`: the step's row of `stats?by=step`
 * @property {number | null} queue_ns
 * @property {number | null} to_first_byte_ns
 * @property {number | null} to_first_token_ns
 * @property {number | null} decode_ns
 * @property {number | null} model_ns
 * @property {number | null} tok_s
 * @property {number | null} cache_hit
 */

// The threads' order key for steps in flight, which have no thread until answered.
export const IN_FLIGHT = -1;

export class Steps {
  /** @param {StepsReply} reply */
  constructor(reply) {
    this.reply = reply;
    this.cols = reply.steps;
    /** @type {Map<number, number>} the row of each request */
    this.rows = new Map();
    /** @type {Map<number, number[]>} the rows of each thread, by its root, `IN_FLIGHT` last */
    this.threads = new Map();
    /** @type {Map<number, number | null>} each thread's span, by its root, as the server gives it */
    this.spans = new Map();
    this.takeSpans(reply);
    this.index();
  }

  /** @param {StepsReply} reply */
  takeSpans(reply) {
    const { thread, span_ns } = reply.thread_spans;
    for (let at = 0; at < thread.length; at++) this.spans.set(thread[at], span_ns[at]);
  }

  get length() {
    return this.cols.request.length;
  }

  index() {
    this.rows.clear();
    this.threads.clear();
    const { request, thread } = this.cols;
    /** @type {Map<number, number[]>} */
    const threads = new Map();
    for (let row = 0; row < request.length; row++) {
      this.rows.set(request[row], row);
      const root = thread[row] ?? IN_FLIGHT;
      const of = threads.get(root);
      if (of) of.push(row);
      else threads.set(root, [row]);
    }
    const roots = [...threads.keys()].sort((a, b) => (a === IN_FLIGHT ? 1 : b === IN_FLIGHT ? -1 : a - b));
    for (const root of roots) this.threads.set(root, /** @type {number[]} */ (threads.get(root)));
  }

  // `reply`, a whole `steps` reply, cut to the rows that are new or differ from those held, as a
  // `since` reply would send them.
  /** @param {StepsReply} reply @returns {StepsReply} */
  since(reply) {
    const cols = reply.steps;
    const columns = /** @type {Array<keyof Columns>} */ (Object.keys(cols));
    /** @param {any} a @param {any} b */
    const same = (a, b) => a === b || (typeof a === "object" && typeof b === "object" && JSON.stringify(a) === JSON.stringify(b));
    const rows = cols.request.flatMap((request, at) => {
      const row = this.rows.get(request);
      const held = row !== undefined && columns.every((column) => same(/** @type {any[]} */ (cols[column])[at] ?? null, /** @type {any[]} */ (this.cols[column] || [])[row] ?? null));
      return held ? [] : [at];
    });
    const steps = /** @type {Columns} */ (Object.fromEntries(columns.map((column) => [column, rows.map((at) => /** @type {any[]} */ (cols[column])[at])])));
    return { ...reply, steps };
  }

  // Takes a `since` reply's rows: a request already held has its row replaced, a new one is
  // placed by its request. Returns the requests whose rows came.
  /** @param {StepsReply} reply @returns {number[]} */
  merge(reply) {
    const came = reply.steps.request;
    const columns = /** @type {Array<keyof Columns>} */ (Object.keys(this.cols));
    let added = false;
    for (let at = 0; at < came.length; at++) {
      const row = this.rows.get(came[at]);
      for (const column of columns) {
        const into = /** @type {any[]} */ (this.cols[column]);
        const value = /** @type {any[]} */ (reply.steps[column] || [])[at] ?? null;
        if (row === undefined) into.push(value);
        else into[row] = value;
      }
      if (row === undefined) (this.rows.set(came[at], this.cols.request.length - 1), (added = true));
    }
    const request = this.cols.request;
    if (added && request.some((n, row) => row > 0 && request[row - 1] > n)) {
      const order = request.map((_, row) => row).sort((a, b) => request[a] - request[b]);
      for (const column of columns) {
        const was = /** @type {any[]} */ (this.cols[column]);
        /** @type {any} */ (this.cols)[column] = order.map((row) => was[row]);
      }
    }
    this.takeSpans(reply);
    this.reply = { ...reply, steps: this.cols };
    this.index();
    return came;
  }

  /** @param {number} request */
  row(request) {
    return this.rows.get(request);
  }

  // The step's place in its thread, from 1, and the thread's steps.
  /** @param {number} row */
  place(row) {
    const of = this.threads.get(this.cols.thread[row] ?? IN_FLIGHT) || [];
    return { at: of.indexOf(row) + 1, of: of.length };
  }

  /** @param {number} row */
  inFlight(row) {
    return this.cols.end_ns[row] === null && this.cols.status[row] === null && this.cols.answer[row] === null;
  }

  // The tone of a step's mark: live in flight, failed, refused (a warning), or ok.
  /** @param {number} row */
  tone(row) {
    const { status, failed, answer } = this.cols;
    if (this.inFlight(row)) return "live";
    if (failed[row] || (status[row] !== null && (/** @type {number} */ (status[row]) < 200 || /** @type {number} */ (status[row]) >= 300))) return "failed";
    if (answer[row] !== "forwarded") return "warn";
    return "finished";
  }

  // Why a step failed, in a word: its status if not 2xx, its stated failure, or its answer.
  /** @param {number} row */
  why(row) {
    const { status, failed, answer } = this.cols;
    const code = status[row];
    if (code !== null && (code < 200 || code >= 300)) return String(code);
    if (failed[row]) return "failed: " + (failed[row]?.code ?? "no code given");
    return answer[row] ?? "no answer";
  }

  // A step's calls in a line: `exec_command×6 write_stdin`.
  /** @param {number} row */
  calls(row) {
    /** @type {Map<string, number>} */
    const counts = new Map();
    for (const name of this.cols.calls[row] || []) counts.set(name, (counts.get(name) || 0) + 1);
    return [...counts].map(([name, n]) => (n > 1 ? name + "×" + n : name)).join(" ");
  }

  // What a step repeats, in words: "Retry 2 of request 4410 after 429", "The same conversation as
  // request 4402"; null for neither.
  /** @param {number} row */
  again(row) {
    const { retry_of, retry_count, repeat_of } = this.cols;
    const of = retry_of[row];
    if (of !== null) {
      const earlier = this.row(of);
      const nth = retry_count[row] ?? 1;
      return "Retry" + (nth > 1 ? " " + nth : "") + " of request " + of + (earlier === undefined ? "" : " after " + this.why(earlier));
    }
    const same = repeat_of[row];
    if (same !== null) return "The same conversation as request " + same;
    return null;
  }

  // A step's tokens as cells: `18.4k tok in`, `82% cached`, `171 out`.
  /** @param {number} row @returns {[string, string, string]} */
  tokens(row) {
    const { input, cache_hit, output } = this.cols;
    if (input[row] === null && output[row] === null) return ["-", "", ""];
    const hit = cache_hit[row];
    const share = hit === null ? "" : Math.round(100 * hit) + "% cached";
    return [count(input[row], "tok") + " in", share, count(output[row], "out")];
  }

  // Why a time of the step is unknown: it is in flight, it never reached a model, the route ended
  // or it was cut before that time came, or its record's build didn't time it.
  /** @param {number} row @param {boolean} token whether the time is of a first token */
  untimed(row, token = false) {
    const answer = this.cols.answer[row];
    if (this.inFlight(row)) return "in flight";
    if (answer === "barred") return "the route barred it, so it reached no model";
    if (answer === "abandoned") return "the route ended before its response came";
    if (answer === "cut") return token && this.cols.first_byte_ns[row] !== null ? "it was cut short before its output began" : "it was cut short before any of its response was kept";
    return token ? "this record's build didn't time first tokens" : "this record's build didn't time it";
  }

  // The step's times on the run's clock, in seconds, each with why it is unknown when it is.
  /** @param {number} row */
  times(row) {
    const c = this.cols;
    /** @param {string | null} nanos @param {boolean} [token] @returns {Timed} */
    const at = (nanos, token) => {
      const value = seconds(nanos);
      return { value, why: value === null ? this.untimed(row, token) : "" };
    };
    return {
      start: at(c.start_ns[row]),
      firstByteAt: at(c.first_byte_ns[row]),
      firstTokenAt: at(c.first_token_ns[row], true),
      end: at(c.end_ns[row]),
      model: /** @type {Timed} */ ({ value: c.model_ns[row] === null ? null : /** @type {number} */ (c.model_ns[row]) / 1e9, why: c.model_ns[row] === null ? this.untimed(row) : "" }),
    };
  }

  // The step's spans and rate as the server measures them (`stats`), in seconds and tokens a
  // second, each with why it is unknown when it is.
  /** @param {number} row @param {StepStats} stats */
  measured(row, stats) {
    const c = this.cols;
    /** @param {number | null} ns @param {string} why @returns {Timed} */
    const timed = (ns, why) => (ns === null ? { value: null, why } : { value: ns / 1e9, why: "" });
    const firstToken = c.first_token_ns[row] === null;
    const decode = timed(stats.decode_ns, this.untimed(row, firstToken));
    const parent = c.parent[row] === null ? undefined : this.row(/** @type {number} */ (c.parent[row]));
    const queueWhy = parent === undefined ? "the thread's first step has no parent" : c.start_ns[row] === null ? this.untimed(row) : "its parent has no end";
    const rateWhy = decode.value === null ? decode.why : c.output[row] === null ? "no output tokens given" : "decoding took under 100 ms, which measures the network, not the model";
    return {
      queue: timed(stats.queue_ns, queueWhy),
      queueIs: parent !== undefined && (c.calls[parent] || []).length > 0 ? "tools and the harness" : "the harness",
      firstByte: timed(stats.to_first_byte_ns, this.untimed(row)),
      ttft: timed(stats.to_first_token_ns, this.untimed(row, firstToken)),
      decode,
      model: timed(stats.model_ns, this.untimed(row)),
      rate: /** @type {Timed} */ (stats.tok_s === null ? { value: null, why: rateWhy } : { value: stats.tok_s, why: "" }),
    };
  }
}

/** @typedef {{ value: number | null, why: string }} Timed a time or span, or why it is unknown */
