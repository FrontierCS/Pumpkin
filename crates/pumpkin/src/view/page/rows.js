// A list of one-line rows of a fixed height that draws only the rows in view and a margin
// around them. The rows are drawn again only when the stretch in view, the items or the selection
// change.

import { h } from "./h.js";

// The rows drawn past those in view.
const OVERSCAN_ROWS = 20;

/** @template T */
export class RowList {
  // `draw(item, index)` makes an item's row; `pick(item, index)` is called when one is clicked;
  // `seen(first, end)`, if given, after each drawing, with the items drawn.
  /** @param {(item: T, index: number) => HTMLElement} draw @param {(item: T, index: number) => void} pick @param {((first: number, end: number) => void) | null} [seen] */
  constructor(draw, pick, seen = null) {
    /** @type {T[]} */
    this.items = [];
    this.selected = -1;
    this.draw = draw;
    this.pick = pick;
    this.seen = seen;
    // Whether the list keeps to its end as items are added: a reader at the end stays there.
    this.tail = false;
    // The stretch last drawn, which a scroll within it doesn't draw again.
    this.drawn = { first: -1, end: -1 };
    // The scroll's top at the last scroll, which tells a scroll up from one down.
    this.scrolled = 0;
    // The height of a row in px, `--row` in the style.
    this.px = parseFloat(getComputedStyle(document.documentElement).getPropertyValue("--row"));
    this.rows = h("div", { class: "rows" });
    this.node = h("div", { class: "list", tabindex: 0 }, this.rows);
    this.node.addEventListener("scroll", () => {
      // Only a scroll up leaves the end: the list's own scroll to its end is read after any
      // change of its height since, which can leave it short of the end.
      const top = this.node.scrollTop;
      this.tail = top < this.scrolled ? this.atTail() : this.tail || this.atTail();
      this.scrolled = top;
      this.paint(false);
    });
  }

  atTail() {
    const node = this.node;
    return node.scrollTop + node.clientHeight >= node.scrollHeight - 2;
  }

  // The index of the first item in view.
  top() {
    return Math.floor(this.node.scrollTop / this.px);
  }

  // Shows `items`, the one at `selected` selected, keeping to the end if the reader was there,
  // else with the item at `anchor` (if given) at the place in view it had.
  /** @param {T[]} items @param {number} selected @param {{ from: number, to: number } | null} [anchor] */
  set(items, selected, anchor = null) {
    const shift = anchor ? (anchor.to - anchor.from) * this.px : 0;
    this.items = items;
    this.selected = selected;
    this.rows.style.setProperty("--count", String(items.length));
    if (this.tail) this.node.scrollTop = this.node.scrollHeight;
    else if (shift) this.node.scrollTop += shift;
    this.paint(true);
  }

  // Goes to the end, and keeps to it.
  end() {
    this.tail = true;
    this.node.scrollTop = this.node.scrollHeight;
    this.paint(true);
  }

  /** @param {number} index @param {boolean} scroll */
  select(index, scroll) {
    this.selected = index;
    if (scroll && index >= 0) {
      const top = index * this.px;
      if (top < this.node.scrollTop || top > this.node.scrollTop + this.node.clientHeight - this.px) this.node.scrollTop = top - this.node.clientHeight / 2;
    }
    this.paint(true);
  }

  // The items in view, and a row on either side, from `first` to before `end`.
  inView() {
    const top = this.top();
    return { first: Math.max(0, top - 2), end: Math.min(this.items.length, top + Math.ceil(this.node.clientHeight / this.px) + 2) };
  }

  // Draws the rows in view and the margin around them, if `again` or the stretch has changed.
  /** @param {boolean} [again] */
  paint(again = true) {
    const inView = Math.ceil(this.node.clientHeight / this.px);
    const top = this.top();
    const count = this.items.length;
    if (this.seen) {
      const { first, end } = this.inView();
      this.seen(first, end);
    }
    // A scroll that keeps the rows in view, and a margin, among those drawn draws nothing.
    if (!again && Math.max(0, top - OVERSCAN_ROWS / 2) >= this.drawn.first && Math.min(count, top + inView + OVERSCAN_ROWS / 2) <= this.drawn.end) return;
    const first = Math.max(0, top - OVERSCAN_ROWS);
    const end = Math.min(this.items.length, top + inView + OVERSCAN_ROWS);
    const rows = [];
    for (let index = first; index < end; index++) {
      const row = this.draw(this.items[index], index);
      row.classList.add("row");
      if (index === this.selected) row.classList.add("on");
      row.style.top = index * this.px + "px";
      row.addEventListener("click", () => this.pick(this.items[index], index));
      rows.push(row);
    }
    this.rows.replaceChildren(...rows);
    this.drawn = { first, end };
  }
}
