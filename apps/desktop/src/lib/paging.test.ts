/**
 * A pager that is off by one is worse than no pager: it hides rows while
 * claiming to show all of them. These pin the boundaries where that happens —
 * an empty list, an exact multiple of the page size, and a total that shrinks
 * under the page the reader is on.
 */
import { describe, expect, it } from "vitest";

import { clampPage, offsetOf, pageCount } from "./paging";

describe("pageCount", () => {
  it("is one page for an empty list, never zero", () => {
    // "Page 1 of 0" is not a thing a reader can be on.
    expect(pageCount(0, 25)).toBe(1);
  });

  it("does not add an empty page at an exact multiple", () => {
    expect(pageCount(50, 25)).toBe(2);
    expect(pageCount(51, 25)).toBe(3);
  });

  it("counts a partial last page", () => {
    expect(pageCount(412, 25)).toBe(17);
  });

  it("survives a nonsense page size rather than dividing by zero", () => {
    expect(pageCount(10, 0)).toBe(1);
  });
});

describe("offsetOf", () => {
  it("starts the first page at zero", () => {
    expect(offsetOf(0, 25)).toBe(0);
  });

  it("skips whole pages", () => {
    expect(offsetOf(16, 25)).toBe(400);
  });

  it("treats a negative page as the first", () => {
    expect(offsetOf(-3, 25)).toBe(0);
  });
});

describe("clampPage", () => {
  it("leaves a page that exists alone", () => {
    expect(clampPage(3, 412, 25)).toBe(3);
  });

  it("pulls back to the last page when the list shrinks", () => {
    // The case this exists for: sitting on page 17 of 412 jobs when the
    // history drops to 30. Page 16 no longer exists; page 1 does.
    expect(clampPage(16, 30, 25)).toBe(1);
  });

  it("lands on the only page when the list empties", () => {
    expect(clampPage(16, 0, 25)).toBe(0);
  });

  it("refuses to go below the first page", () => {
    expect(clampPage(-1, 412, 25)).toBe(0);
  });
});
