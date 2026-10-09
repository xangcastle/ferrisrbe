import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { RefreshCwIcon, TargetIcon } from "lucide-react";
import { getTargets } from "@/api/bes";
import StatusBadge from "@/components/StatusBadge";
import Pager from "@/components/Pager";
import {
  EmptyState,
  OutcomeBar,
  PageHeader,
  SearchInput,
} from "@/components/page";
import { useDebounced } from "@/hooks/useDebounced";
import type { TargetSummary } from "@/types";
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
import { Tabs, TabsList, TabsTrigger } from "@/shadcn/ui/tabs";

const PAGE_SIZE = 50;

const STATUSES = [
  { key: "all", label: "All" },
  { key: "success", label: "Passing" },
  { key: "failure", label: "Failing" },
  { key: "cached", label: "Cached" },
];

export default function Targets() {
  const [targets, setTargets] = useState<TargetSummary[]>([]);
  const [total, setTotal] = useState(0);
  const [page, setPage] = useState(0);
  const [filter, setFilter] = useState("");
  const [status, setStatus] = useState("all");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const navigate = useNavigate();
  const q = useDebounced(filter, 300);

  useEffect(() => {
    setPage(0);
  }, [q, status]);

  useEffect(() => {
    let cancelled = false;
    async function load() {
      try {
        setLoading(true);
        const data = await getTargets({
          limit: PAGE_SIZE,
          offset: page * PAGE_SIZE,
          q: q || undefined,
          status: status === "all" ? undefined : status,
        });
        if (cancelled) return;
        setTargets(data.items);
        setTotal(data.total);
        setError(null);
      } catch (err) {
        if (cancelled) return;
        setError(err instanceof Error ? err.message : "Failed to load targets");
      } finally {
        if (!cancelled) setLoading(false);
      }
    }
    load();
    return () => {
      cancelled = true;
    };
  }, [page, q, status]);

  return (
    <div className="space-y-6">
      <PageHeader
        icon={TargetIcon}
        title="Targets"
        description="Every target with its latest result and its history across builds."
      >
        {loading && targets.length > 0 && (
          <RefreshCwIcon className="size-4 animate-spin text-muted-foreground" />
        )}
      </PageHeader>

      <Card className="gap-0 py-0">
        <div className="flex flex-col gap-3 border-b px-6 py-4 lg:flex-row lg:items-center lg:justify-between">
          <Tabs value={status} onValueChange={setStatus}>
            <TabsList>
              {STATUSES.map(({ key, label }) => (
                <TabsTrigger key={key} value={key}>
                  {label}
                </TabsTrigger>
              ))}
            </TabsList>
          </Tabs>
          <SearchInput
            value={filter}
            onChange={setFilter}
            placeholder="Label or rule kind…"
            className="lg:w-80"
          />
        </div>
        <CardContent className="px-0">
          {error ? (
            <p className="px-6 py-6 text-sm text-destructive">{error}</p>
          ) : loading && targets.length === 0 ? (
            <div className="space-y-2 p-6">
              {Array.from({ length: 8 }, (_, i) => (
                <Skeleton key={i} className="h-10" />
              ))}
            </div>
          ) : targets.length === 0 ? (
            <EmptyState icon={TargetIcon} title="No targets found">
              {filter || status !== "all"
                ? "Nothing matches these filters."
                : "Targets are recorded when a build finishes."}
            </EmptyState>
          ) : (
            <Table>
              <TableHeader className="bg-muted/40">
                <TableRow>
                  <TableHead className="pl-6">Target</TableHead>
                  <TableHead>Latest</TableHead>
                  <TableHead className="text-right">Runs</TableHead>
                  <TableHead>Passed / failed / cached</TableHead>
                  <TableHead className="text-right">Actions</TableHead>
                  <TableHead className="pr-6">Tags</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {targets.map((target) => (
                  <TableRow
                    key={target.label}
                    className="cursor-pointer"
                    onClick={() =>
                      navigate(`/targets/${encodeURIComponent(target.label)}`)
                    }
                  >
                    <TableCell className="max-w-md pl-6">
                      <div
                        className="truncate font-mono text-xs font-medium"
                        title={target.label}
                      >
                        {target.label}
                      </div>
                      <div className="truncate text-xs text-muted-foreground">
                        {target.target_kind || "–"}
                      </div>
                    </TableCell>
                    <TableCell>
                      <StatusBadge status={target.latest_status} />
                    </TableCell>
                    <TableCell className="text-right tabular-nums">
                      {target.total_executions.toLocaleString()}
                    </TableCell>
                    <TableCell>
                      <OutcomeBar
                        parts={[
                          {
                            label: "passed",
                            count: target.success_count,
                            className: "bg-success",
                          },
                          {
                            label: "failed",
                            count: target.failure_count,
                            className: "bg-destructive",
                          },
                          {
                            label: "cached",
                            count: target.cached_count,
                            className: "bg-chart-2",
                          },
                        ]}
                      />
                    </TableCell>
                    <TableCell className="text-right text-xs tabular-nums">
                      {target.action_count > 0 ? (
                        <>
                          {target.action_count.toLocaleString()}
                          <span className="text-muted-foreground">
                            {" "}
                            · {target.cached_actions.toLocaleString()} cached
                          </span>
                          {target.failed_actions > 0 && (
                            <span className="text-destructive">
                              {" "}
                              · {target.failed_actions.toLocaleString()} failed
                            </span>
                          )}
                        </>
                      ) : (
                        <span className="text-muted-foreground">–</span>
                      )}
                    </TableCell>
                    <TableCell className="pr-6">
                      <div className="flex flex-wrap gap-1">
                        {target.tags.slice(0, 3).map((tag) => (
                          <Badge key={tag} variant="secondary">
                            {tag}
                          </Badge>
                        ))}
                        {target.tags.length > 3 && (
                          <Badge variant="outline">
                            +{target.tags.length - 3}
                          </Badge>
                        )}
                      </div>
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
