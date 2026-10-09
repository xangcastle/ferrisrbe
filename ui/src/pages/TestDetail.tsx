import { useEffect, useMemo, useState } from "react";
import { Link, useParams } from "react-router-dom";
import {
  ArrowLeftIcon,
  CheckCircle2Icon,
  ClockIcon,
  DatabaseIcon,
  FlaskConicalIcon,
  HistoryIcon,
  RepeatIcon,
} from "lucide-react";
import { cn } from "cn";
import { getTestHistory } from "@/api/bes";
import StatusBadge from "@/components/StatusBadge";
import { EmptyState, PageHeader, Stat } from "@/components/page";
import { formatDuration, formatPercent, timeAgo } from "@/lib/format";
import type { TestExecution } from "@/types";
import { Badge } from "@/shadcn/ui/badge";
import { Button } from "@/shadcn/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/shadcn/ui/card";
import { Skeleton } from "@/shadcn/ui/skeleton";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/shadcn/ui/table";

const STRIP_RUNS = 60;

function stripColor(status: string) {
  if (status === "PASSED") return "bg-success";
  if (status === "FLAKY") return "bg-warning";
  if (status === "NO_STATUS") return "bg-muted-foreground/40";
  return "bg-destructive";
}

/** One tick per run, oldest on the left: a test's health at a glance. */
function RunStrip({ history }: { history: TestExecution[] }) {
  const runs = history.slice(-STRIP_RUNS);
  return (
    <div className="flex h-8 items-stretch gap-0.5">
      {runs.map((ex, i) => (
        <Link
          key={`${ex.invocation_id}-${i}`}
          to={`/builds/${encodeURIComponent(ex.invocation_id)}`}
          title={`${ex.status} · ${formatDuration(ex.duration_ms)} · ${ex.start_time ? new Date(ex.start_time).toLocaleString() : ""}`}
          className={cn(
            "max-w-3 flex-1 rounded-sm opacity-80 transition-opacity hover:opacity-100",
            stripColor(ex.status),
          )}
        />
      ))}
    </div>
  );
}

export default function TestDetail() {
  const { label = "" } = useParams<{ label: string }>();
  const [history, setHistory] = useState<TestExecution[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    getTestHistory(label)
      .then((data) => !cancelled && setHistory(data))
      .catch(
        (err) =>
          !cancelled &&
          setError(
            err instanceof Error ? err.message : "Failed to load history",
          ),
      )
      .finally(() => !cancelled && setLoading(false));
    return () => {
      cancelled = true;
    };
  }, [label]);

  const summary = useMemo(() => {
    const passed = history.filter((h) => h.status === "PASSED").length;
    const cached = history.filter(
      (h) => h.cached_locally || h.cached_remotely,
    ).length;
    const ran = history.filter(
      (h) => !h.cached_locally && !h.cached_remotely && h.duration_ms > 0,
    );
    return {
      passRate: history.length ? passed / history.length : null,
      cached,
      avg: ran.length
        ? Math.round(ran.reduce((s, h) => s + h.duration_ms, 0) / ran.length)
        : null,
      latest: history[history.length - 1],
    };
  }, [history]);

  return (
    <div className="space-y-6">
      <PageHeader
        icon={FlaskConicalIcon}
        title={<span className="font-mono text-xl">{label}</span>}
        description="Results across retained builds"
      >
        <Button variant="outline" size="sm" asChild>
          <Link to="/tests">
            <ArrowLeftIcon /> Tests
          </Link>
        </Button>
      </PageHeader>

      {loading ? (
        <Skeleton className="h-72 rounded-xl" />
      ) : error ? (
        <p className="text-sm text-destructive">{error}</p>
      ) : history.length === 0 ? (
        <Card>
          <EmptyState icon={HistoryIcon} title="No runs for this test">
            It shows up once a bazel test reports it.
          </EmptyState>
        </Card>
      ) : (
        <>
          <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
            <Stat
              icon={RepeatIcon}
              label="Runs"
              value={history.length.toLocaleString()}
            />
            <Stat
              icon={CheckCircle2Icon}
              label="Pass rate"
              value={formatPercent(summary.passRate)}
            />
            <Stat
              icon={ClockIcon}
              label="Average run (uncached)"
              value={formatDuration(summary.avg)}
            />
            <Stat
              icon={DatabaseIcon}
              label="Served from cache"
              value={summary.cached.toLocaleString()}
            />
          </div>

          <Card className="gap-4">
            <CardHeader>
              <CardTitle>Recent runs</CardTitle>
              <CardDescription>
                The last {Math.min(history.length, STRIP_RUNS)} results, oldest
                first; each opens its build. Latest:{" "}
                {timeAgo(summary.latest?.start_time)}.
              </CardDescription>
            </CardHeader>
            <CardContent>
              <RunStrip history={history} />
            </CardContent>
          </Card>

          <Card className="gap-0 pb-0">
            <CardHeader className="pb-4">
              <CardTitle>Runs</CardTitle>
              <CardDescription>Newest first.</CardDescription>
            </CardHeader>
            <CardContent className="px-0">
              <div className="max-h-[36rem] overflow-auto border-t">
                <Table>
                  <TableHeader className="sticky top-0 bg-card">
                    <TableRow>
                      <TableHead className="pl-6">Build</TableHead>
                      <TableHead>Status</TableHead>
                      <TableHead>Cache</TableHead>
                      <TableHead className="text-right">Duration</TableHead>
                      <TableHead className="pr-6 text-right">Started</TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {[...history].reverse().map((ex, i) => (
                      <TableRow key={`${ex.invocation_id}-${i}`}>
                        <TableCell className="pl-6">
                          <Link
                            to={`/builds/${encodeURIComponent(ex.invocation_id)}`}
                            className="font-mono text-xs hover:text-primary"
                          >
                            {ex.invocation_id.slice(0, 13)}…
                          </Link>
                        </TableCell>
                        <TableCell>
                          <StatusBadge status={ex.status} />
                        </TableCell>
                        <TableCell>
                          {ex.cached_remotely ? (
                            <Badge variant="secondary">remote cache</Badge>
                          ) : ex.cached_locally ? (
                            <Badge variant="secondary">local cache</Badge>
                          ) : (
                            <span className="text-muted-foreground">ran</span>
                          )}
                        </TableCell>
                        <TableCell className="text-right tabular-nums">
                          {formatDuration(ex.duration_ms)}
                        </TableCell>
                        <TableCell className="pr-6 text-right text-muted-foreground">
                          {timeAgo(ex.start_time)}
                        </TableCell>
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              </div>
            </CardContent>
          </Card>
        </>
      )}
    </div>
  );
}
