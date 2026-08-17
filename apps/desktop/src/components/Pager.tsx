import { ChevronLeft, ChevronRight } from "lucide-react";

import { pageCount } from "@/lib/paging";
import { cn } from "@/lib/utils";

/**
 * Prev/Next paging for a list that is fetched a page at a time.
 *
 * Numbered rather than "load more": the lists it serves — job history, the
 * change log — are read to answer "what happened around then", which means
 * going back as often as forward, and a list that only grows cannot go back.
 *
 * `page` is 0-based here and 1-based on screen, because the caller does
 * arithmetic with it and the reader does not.
 */
export default function Pager({
  page,
  pageSize,
  total,
  onPage,
  noun,
}: {
  page: number;
  pageSize: number;
  total: number;
  onPage: (page: number) => void;
  /** Plural noun for the total, e.g. `jobs`. */
  noun: string;
}) {
  const pages = pageCount(total, pageSize);
  // A single page needs no controls, but the total is still worth saying.
  const first = page <= 0;
  const last = page >= pages - 1;

  return (
    <div className="mt-2 flex items-center justify-between gap-4 text-xs text-slate-500">
      <span className="tabular-nums">
        {total.toLocaleString()} {noun}
      </span>

      {pages > 1 && (
        <div className="flex items-center gap-1">
          <PagerButton
            onClick={() => onPage(page - 1)}
            disabled={first}
            label="Previous page"
          >
            <ChevronLeft className="h-3.5 w-3.5" />
            Prev
          </PagerButton>

          <span className="px-2 tabular-nums text-slate-400">
            Page {page + 1} of {pages}
          </span>

          <PagerButton
            onClick={() => onPage(page + 1)}
            disabled={last}
            label="Next page"
          >
            Next
            <ChevronRight className="h-3.5 w-3.5" />
          </PagerButton>
        </div>
      )}
    </div>
  );
}

function PagerButton({
  onClick,
  disabled,
  label,
  children,
}: {
  onClick: () => void;
  disabled: boolean;
  label: string;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      title={label}
      aria-label={label}
      className={cn(
        "flex items-center gap-1 rounded-md border border-slate-700 px-2 py-1 transition",
        disabled
          ? "cursor-default opacity-30"
          : "text-slate-300 hover:bg-slate-800",
      )}
    >
      {children}
    </button>
  );
}
