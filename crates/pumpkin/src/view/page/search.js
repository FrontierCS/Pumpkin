// A trace's search in the static mode, where no file answers `search?q=`: the export's chunks of
// the trace's text, each in the form that the server's search matches in (`searchable` in
// `crates/pumpkin/src/view/trace/json.rs`), read on the first search and kept for the next, and
// scanned for the query as a plain substring, step by step in the order of their requests, as
// the server scans them, so that the hits are the server's.

import { readFile } from "./h.js";
import { fold } from "./inspector.js";

/**
 * @typedef {{ at: string, request: number, index: number, kind: string, byte: number }} Hit
 * @typedef {{ steps: Array<{ request: number, thread: number | null, entries: Array<[number, string, string, string]> }>, next: number | null }} Chunk
 */

// The chunks read ahead of the one scanned, at most.
const CHUNKS_AT_ONCE = 4;
// The hits given with their places, as many as the server's reply gives (`SEARCH_HITS`).
const HITS = 1000;

export class Chunks {
  /** @param {string} id @param {string} caller */
  constructor(id, caller) {
    this.dir = "runs/" + id + "/trace/" + caller + "/search/";
    /** @type {Array<Promise<Chunk>>} each chunk read or being read, by its number */
    this.read = [];
    /** @type {number | null} how many chunks there are, as the first chunk and the steps say */
    this.count = null;
    /** @type {number | null} the number of the last chunk read, which grows while the trace does */
    this.last = null;
  }

  /** @param {number} k */
  chunk(k) {
    let read = this.read[k];
    if (!read) {
      const reading = readFile(this.dir + k + ".json").then((response) => /** @type {Promise<Chunk>} */ (response.json()));
      reading.catch(() => {
        if (this.read[k] === reading) this.read.length = Math.min(this.read.length, k);
      });
      this.read[k] = read = reading;
    }
    return read;
  }

  // Forgets the last chunk and those after it, as the trace has grown.
  forgetLast() {
    if (this.last !== null) this.read.length = Math.min(this.read.length, this.last);
    this.last = null;
    this.count = null;
  }

  // Scans every chunk for `needle` in the entries of `kind` and of `thread` if given, in order,
  // reading ahead of the chunk scanned; after each chunk calls `found` with the hits so far,
  // their count, and how many chunks are left. Stops, giving false, once `wanted` turns false.
  // `steps` is how many steps the trace has.
  /**
   * @param {string} needle @param {{ kind: string, thread: number | null }} filter @param {number} steps
   * @param {() => boolean} wanted @param {(hits: Hit[], count: number, left: number) => void} found
   */
  async search(needle, { kind, thread }, steps, wanted, found) {
    const query = fold(needle);
    const encoder = new TextEncoder();
    /** @type {Hit[]} */
    const hits = [];
    let count = 0;
    for (let k = /** @type {number | null} */ (0); k !== null; ) {
      const ahead = this.count === null ? k + 1 : Math.min(this.count, k + CHUNKS_AT_ONCE);
      for (let at = k; at < ahead; at++) this.chunk(at);
      const chunk = await this.chunk(k);
      if (!wanted()) return false;
      if (k === 0) this.count = chunk.next === null ? 1 : Math.ceil(steps / chunk.steps.length);
      if (chunk.next === null) this.last = k;
      for (const step of chunk.steps) {
        if (thread !== null && step.thread !== thread) continue;
        for (const [index, at, kindOf, text] of step.entries) {
          if (kind && kind !== kindOf) continue;
          // A hit's place is in UTF-8 bytes, as the server's; the scan is in UTF-16 units.
          let [unit, byte] = [0, 0];
          for (let place = text.indexOf(query); place >= 0; place = text.indexOf(query, place + query.length)) {
            count++;
            if (hits.length === HITS) continue;
            byte += encoder.encode(text.slice(unit, place)).length;
            unit = place;
            hits.push({ at, request: step.request, index, kind: kindOf, byte });
          }
        }
      }
      k = chunk.next;
      found(hits, count, k === null ? 0 : Math.max(1, (this.count ?? 0) - k));
      // The browser draws, and takes keys and scrolls, between chunks.
      if (k !== null) await new Promise((resolve) => setTimeout(resolve, 0));
    }
    return true;
  }
}
