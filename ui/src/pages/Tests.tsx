import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { FlaskConicalIcon, RefreshCwIcon } from "lucide-react";
import { getTests } from "@/api/bes";
import Pager from "@/components/Pager";
import StatusBadge from "@/components/StatusBadge";
import {
  EmptyState,
  OutcomeBar,
  PageHeader,
  SearchInput,
} from "@/components/page";
import { useDebounced } from "@/hooks/useDebounced";
import type { TestSummary } from "@/types";
import { Badge } from "@/shadcn/ui/badge";
import { Card, CardContent } from "@/shadcn/ui/card";
import { Skeleton } from "@/shadcn/ui/skeleton";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/shadcn/ui/table";

const PAGE_SIZE = 50;

export default function Tests() {
  const [tests, setTests] = useState<TestSummary[]>([]);
  const [total, setTotal] = useState(0);
  const [page, setPage] = useState(0);
  const [filter, setFilter] = useState("");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const navigate = useNavigate();
  const q = useDebounced(filter, 300);

  useEffect(() => {
    setPage(0);
  }, [q]);

  useEffect(() => {
    let cancelled = false;
    async function load() {
      try {
        setLoading(true);
        const data = await getTests({
          limit: PAGE_SIZE,
          offset: page * PAGE_SIZE,
          q: q || undefined,
        });
        if (cancelled) return;
        setTests(data.items);
        setTotal(data.total);
        setError(null);
      } catch (err) {
        if (cancelled) return;
        setError(err instanceof Error ? err.message : "Failed to load tests");
      } finally {
        if (!cancelled) setLoading(false);
      }
    }
    load();
    return () => {
      cancelled = true;
    };
  }, [page, q]);

  return (
    <div className="space-y-6">
      <PageHeader
        icon={FlaskConicalIcon}
        title="Tests"
        description="Every test with its latest result, flakiness and history."
      >
        {loading && tests.length > 0 && (
          <RefreshCwIcon className="size-4 animate-spin text-muted-foreground" />
        )}
      </PageHeader>

      <Card className="gap-0 py-0">
        <div className="flex items-center justify-between gap-3 border-b px-6 py-4">
          <p className="text-sm text-muted-foreground tabular-nums">
            {total.toLocaleString()} tests
          </p>
          <SearchInput
            value={filter}
            onChange={setFilter}
            placeholder="Filter by test label…"
            className="sm:w-80"
          />
        </div>
        <CardContent className="px-0">
          {error ? (
            <p className="px-6 py-6 text-sm text-destructive">{error}</p>
          ) : loading && tests.length === 0 ? (
            <div className="space-y-2 p-6">
              {Array.from({ length: 8 }, (_, i) => (
                <Skeleton key={i} className="h-10" />
              ))}
            </div>
          ) : tests.length === 0 ? (
            <EmptyState icon={FlaskConicalIcon} title="No tests found">
              {filter
                ? "Nothing matches the filter."
                : "Tests show up after a bazel test streams to this server."}
            </EmptyState>
          ) : (
            <Table>
              <TableHeader className="bg-muted/40">
                <TableRow>
                  <TableHead className="pl-6">Test</TableHead>
                  <TableHead>Latest</TableHead>
                  <TableHead className="text-right">Runs</TableHead>
                  <TableHead>Passed / failed / cached</TableHead>
                  <TableHead className="pr-6 text-right">Flaky</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {tests.map((test) => (
                  <TableRow
                    key={test.label}
                    className="cursor-pointer"
                    onClick={() =>
                      navigate(`/tests/${encodeURIComponent(test.label)}`)
                    }
                  >
                    <TableCell className="max-w-xl pl-6">
                      <div
                        className="truncate font-mono text-xs font-medium"
                        title={test.label}
                      >
                        {test.label}
                      </div>
                    </TableCell>
                    <TableCell>
                      <StatusBadge status={test.latest_status} />
                    </TableCell>
                    <TableCell className="text-right tabular-nums">
                      {test.total_runs.toLocaleString()}
                    </TableCell>
                    <TableCell>
                      <OutcomeBar
                        parts={[
                          {
                            label: "passed",
                            count: test.passed_count,
                            className: "bg-success",
                          },
                          {
                            label: "failed",
                            count: test.failed_count,
                            className: "bg-destructive",
                          },
                          {
                            label: "cached",
                            count: test.cached_count,
                            className: "bg-chart-2",
                          },
                        ]}
                      />
                    </TableCell>
                    <TableCell className="pr-6 text-right">
                      {test.flaky_count > 0 ? (
                        <Badge
                          variant="outline"
                          className="border-warning/40 bg-warning/15"
                        >
                          {test.flaky_count.toLocaleString()}
                        </Badge>
                      ) : (
                        <span className="text-muted-foreground">–</span>
                      )}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
        <Pager
          page={page}
          pageSize={PAGE_SIZE}
          total={total}
          loading={loading}
          onPage={setPage}
        />
      </Card>
    </div>
  );
}
