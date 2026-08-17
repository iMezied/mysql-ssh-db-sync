/**
 * Page arithmetic for lists fetched a page at a time.
 *
 * Separated from the components so the awkward cases — an empty list, a total
 * that shrinks under the page the user is on, an exact multiple of the page
 * size — can be asserted directly rather than inferred from a rendered pager.
 *
 * Pages are 0-based throughout. They become 1-based only where a person reads
 * them.
 */

/**
 * How many pages `total` rows make.
 *
 * Never zero: an empty list is one empty page, not none, so "Page 1 of 0"
 * cannot be rendered and `clampPage` always has somewhere to land.
 */
export function pageCount(total: number, pageSize: number): number {
  if (pageSize <= 0) return 1;
  return Math.max(1, Math.ceil(Math.max(0, total) / pageSize));
}

/** The row offset a page starts at. */
export function offsetOf(page: number, pageSize: number): number {
  return Math.max(0, page) * pageSize;
}

/**
 * The nearest page that actually exists.
 *
 * Used when the list shrinks under the reader — a pruned log, a store replaced
 * by an import — which otherwise leaves them on an empty page that reads as
 * "nothing here" rather than "you are past the end".
 */
export function clampPage(
  page: number,
  total: number,
  pageSize: number,
): number {
  const last = pageCount(total, pageSize) - 1;
  if (!Number.isFinite(page) || page < 0) return 0;
  return Math.min(Math.floor(page), last);
}
