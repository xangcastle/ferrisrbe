import { useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { HammerIcon, InboxIcon, Loader2Icon } from "lucide-react";
import { useLiveBuilds } from "@/hooks/useLive";
import StatusBadge from "@/components/StatusBadge";
import { BuildOrigin, invocationTitle } from "@/components/BuildMeta";
import { EmptyState, PageHeader, SearchInput } from "@/components/page";
import {
  cacheHitRatio,
  formatDuration,
  formatPercent,
  timeAgo,
} from "@/lib/format";
import type { BuildStatus } from "@/types";
import { Button } from "@/shadcn/ui/button";
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

type Filter = "all" | BuildStatus;

const FILTERS: { key: Filter; label: string }[] = [
  { key: "all", label: "All" },
  { key: "in_progress", label: "Running" },
  { key: "success", label: "Passed" },
  { key: "failure", label: "Failed" },
  { key: "interrupted", label: "Interrupted" },
];

export default function Builds() {
  const { builds, total, hasMore, loadMore, loadingMore, loading, error } =
    useLiveBuilds(50);
  const navigate = useNavigate();
  const [filter, setFilter] = useState<Filter>("all");
  const [query, setQuery] = useState("");

  const counts = useMemo(() => {
    const byStatus: Record<string, number> = { all: builds.length };
    for (const b of builds) byStatus[b.status] = (byStatus[b.status] ?? 0) + 1;
    return byStatus;
  }, [builds]);

  const visible = useMemo(() => {
    const q = query.trim().toLowerCase();
    return builds.filter(
      (b) =>
        (filter === "all" || b.status === filter) &&
        (!q ||
          b.invocation_id.toLowerCase().includes(q) ||
          invocationTitle(b).toLowerCase().includes(q) ||
          Object.values(b.metadata ?? {}).some((v) =>
            v.toLowerCase().includes(q),
          )),
    );
  }, [builds, filter, query]);

  return (
    <div className="space-y-6">
      <PageHeader
        icon={HammerIcon}
        title="Builds"
        description={`Every invocation that streamed to this BES · ${total.toLocaleString()} retained`}
      />

      <Card className="gap-0 py-0">
        <div className="flex flex-col gap-3 border-b px-6 py-4 lg:flex-row lg:items-center lg:justify-between">
          <Tabs value={filter} onValueChange={(v) => setFilter(v as Filter)}>
            <TabsList>
              {FILTERS.map(({ key, label }) => (
                <TabsTrigger key={key} value={key}>
                  {label}
                  <span className="text-muted-foreground tabular-nums">
                    {counts[key] ?? 0}
                  </span>
                </TabsTrigger>
              ))}
            </TabsList>
          </Tabs>
          <SearchInput
            value={query}
            onChange={setQuery}
            placeholder="Invocation, pattern, user, branch…"
            className="lg:w-80"
          />
        </div>
        <CardContent className="px-0">
          {loading ? (
            <div className="space-y-2 p-6">
              {Array.from({ length: 8 }, (_, i) => (
                <Skeleton key={i} className="h-10" />
              ))}
            </div>
          ) : error ? (
            <p className="px-6 py-6 text-sm text-destructive">{error}</p>
          ) : visible.length === 0 ? (
            <EmptyState icon={InboxIcon} title="No builds here">
              {builds.length === 0
                ? "Point Bazel at this server with --bes_backend and builds show up live."
                : "Nothing in the loaded builds matches this filter."}
            </EmptyState>
          ) : (
            <Table>
              <TableHeader className="bg-muted/40">
                <TableRow>
                  <TableHead className="pl-6">Status</TableHead>
                  <TableHead>Invocation</TableHead>
                  <TableHead>Origin</TableHead>
                  <TableHead className="text-right">Duration</TableHead>
                  <TableHead className="text-right">Actions</TableHead>
                  <TableHead className="text-right">Cache hits</TableHead>
                  <TableHead className="pr-6 text-right">Started</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {visible.map((build) => (
                  <TableRow
                    key={build.invocation_id}
                    className="cursor-pointer"
                    onClick={() =>
                      navigate(
                        `/builds/${encodeURIComponent(build.invocation_id)}`,
                      )
                    }
                  >
                    <TableCell className="pl-6">
                      <StatusBadge status={build.status} />
                    </TableCell>
                    <TableCell className="max-w-sm">
                      <div className="truncate font-medium">
                        {invocationTitle(build)}
                      </div>
                      <div className="truncate font-mono text-xs text-muted-foreground">
                        {build.invocation_id}
                      </div>
                    </TableCell>
                    <TableCell className="max-w-xs truncate text-sm">
                      <BuildOrigin build={build} />
                    </TableCell>
                    <TableCell className="text-right tabular-nums">
                      {build.status === "in_progress"
                        ? "–"
                        : formatDuration(build.duration_ms)}
                    </TableCell>
                    <TableCell className="text-right tabular-nums">
                      {build.total_actions.toLocaleString()}
                    </TableCell>
                    <TableCell className="text-right tabular-nums">
                      {formatPercent(cacheHitRatio(build))}
                    </TableCell>
                    <TableCell className="pr-6 text-right text-muted-foreground">
                      {timeAgo(build.start_time)}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
        {hasMore && !loading && (
          <div className="flex items-center justify-between border-t px-6 py-3 text-sm text-muted-foreground">
            <span className="tabular-nums">
              {builds.length.toLocaleString()} of {total.toLocaleString()}{" "}
              loaded
            </span>
            <Button
              variant="outline"
              size="sm"
              onClick={loadMore}
              disabled={loadingMore}
            >
              {loadingMore && <Loader2Icon className="animate-spin" />}
              Load more
            </Button>
          </div>
        )}
      </Card>
    </div>
  );
}
