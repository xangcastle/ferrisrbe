import { ChevronLeftIcon, ChevronRightIcon } from "lucide-react";
import { Button } from "@/shadcn/ui/button";

interface PagerProps {
  page: number;
  pageSize: number;
  total: number;
  loading?: boolean;
  onPage: (page: number) => void;
}

/** Offset pager for server-side listings: "1–50 of 15,762" with prev/next. */
export default function Pager({
  page,
  pageSize,
  total,
  loading,
  onPage,
}: PagerProps) {
  if (total <= pageSize) return null;
  const first = page * pageSize + 1;
  const last = Math.min(total, (page + 1) * pageSize);
  const lastPage = Math.max(0, Math.ceil(total / pageSize) - 1);
  return (
    <div className="flex items-center justify-between border-t px-6 py-3 text-sm text-muted-foreground">
      <span className="tabular-nums">
        {first.toLocaleString()}–{last.toLocaleString()} of{" "}
        {total.toLocaleString()}
      </span>
      <div className="flex items-center gap-2">
        <span className="hidden tabular-nums sm:inline">
          Page {page + 1} of {lastPage + 1}
        </span>
        <Button
          variant="outline"
          size="icon"
          className="size-8"
          aria-label="Previous page"
          disabled={loading || page === 0}
          onClick={() => onPage(page - 1)}
        >
          <ChevronLeftIcon />
        </Button>
        <Button
          variant="outline"
          size="icon"
          className="size-8"
          aria-label="Next page"
          disabled={loading || page >= lastPage}
          onClick={() => onPage(page + 1)}
        >
          <ChevronRightIcon />
        </Button>
      </div>
    </div>
  );
}
