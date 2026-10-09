import { useEffect, useMemo, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import {
  ArrowLeftIcon,
  CheckCircle2Icon,
  DatabaseIcon,
  HistoryIcon,
  LockIcon,
  RepeatIcon,
  TargetIcon,
} from "lucide-react";
import { cn } from "cn";
import { getTargetHistory } from "@/api/bes";
import StatusBadge from "@/components/StatusBadge";
import TargetInputs from "@/components/TargetInputs";
import TargetTimeline from "@/components/TargetTimeline";
import { EmptyState, PageHeader, Stat } from "@/components/page";
import { formatDuration, formatPercent, timeAgo } from "@/lib/format";
import type { TargetExecution } from "@/types";
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

const TIMELINE_RUNS = 40;

/**
 * One execution per build. Older records list a target once per
 * configuration and per aspect applied to it; those are folded into the
 * first, failed if any of them failed.
 */
function oncePerBuild(history: TargetExecution[]) {
  const byBuild = new Map<string, TargetExecution>();
  for (const ex of history) {
    const seen = byBuild.get(ex.invocation_id);
    if (!seen) byBuild.set(ex.invocation_id, ex);
    else if (ex.status === "failure" && seen.status !== "failure")
      byBuild.set(ex.invocation_id, { ...seen, status: "failure" });
  }
  return [...byBuild.values()];
}

function barColor(status: string) {
  if (status === "failure") return "bg-destructive";
  if (status === "cached") return "bg-chart-2";
  return "bg-success";
}

/**
 * The latest runs as vertical bars, oldest on the left: action time when
 * Bazel reported the target's actions, otherwise how many of its actions
 * the ActionCache could not serve.
 */
function RunChart({
  runs,
  value,
  format,
}: {
  runs: TargetExecution[];
  value: (ex: TargetExecution) => number;
  format: (n: number) => string;
}) {
  const navigate = useNavigate();
  const max = Math.max(...runs.map(value), 1);
  return (
    <div className="flex h-40 items-end gap-1">
      {runs.map((ex) => (
        <button
          key={ex.invocation_id}
          type="button"
          title={`${format(value(ex))} · ${ex.status} · ${new Date(ex.build_start_time ?? "").toLocaleString()}`}
          onClick={() =>
            navigate(`/builds/${encodeURIComponent(ex.invocation_id)}`)
          }
          className="group flex h-full max-w-12 flex-1 items-end"
        >
          <span
            className={cn(
              "w-full rounded-t-sm opacity-80 transition-opacity group-hover:opacity-100",
              value(ex) === 0 ? "bg-muted-foreground/30" : barColor(ex.status),
            )}
            style={{ height: `${Math.max((value(ex) / max) * 100, 3)}%` }}
          />
        </button>
      ))}
    </div>
  );
}

export default function TargetDetail() {
  const { label = "" } = useParams<{ label: string }>();
  const [raw, setRaw] = useState<TargetExecution[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    getTargetHistory(label)
      .then((data) => !cancelled && setRaw(data))
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

  const history = useMemo(() => oncePerBuild(raw), [raw]);

  const summary = useMemo(() => {
    const failures = history.filter((h) => h.status === "failure").length;
    const hits = history.reduce((s, h) => s + (h.cache_hits ?? 0), 0);
    const misses = history.reduce((s, h) => s + (h.cache_misses ?? 0), 0);
    return {
      passRate: history.length ? 1 - failures / history.length : null,
      hitRate: hits + misses === 0 ? null : hits / (hits + misses),
      timed: history.some((h) => h.action_duration_ms > 0),
      counted: history.some((h) => h.action_count > 0),
      latest: history[history.length - 1],
    };
  }, [history]);

  const runs = history.slice(-TIMELINE_RUNS);
  const latestInputs = [...history].reverse().find((h) => h.heaviest_inputs);
  const env = Object.entries(summary.latest?.env_vars ?? {}).sort(([a], [b]) =>
    a.localeCompare(b),
  );

  return (
    <div className="space-y-6">
      <PageHeader
        icon={TargetIcon}
        title={<span className="font-mono text-xl">{label}</span>}
        description={
          summary.latest?.target_kind
            ? `${summary.latest.target_kind} · history across retained builds`
            : "History across retained builds"
        }
      >
        <Button variant="outline" size="sm" asChild>
          <Link to="/targets">
            <ArrowLeftIcon /> Targets
          </Link>
        </Button>
      </PageHeader>

      {loading ? (
        <Skeleton className="h-72 rounded-xl" />
      ) : error ? (
        <p className="text-sm text-destructive">{error}</p>
      ) : history.length === 0 ? (
        <Card>
          <EmptyState icon={HistoryIcon} title="No history for this target">
            It shows up once a finished build reports it.
          </EmptyState>
        </Card>
      ) : (
        <>
          <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
            <Stat
              icon={RepeatIcon}
              label="Builds"
              value={history.length.toLocaleString()}
            />
            <Stat
              icon={CheckCircle2Icon}
              label="Pass rate"
              value={formatPercent(summary.passRate)}
            />
            <Stat
              icon={DatabaseIcon}
              label="Action cache hit rate"
              value={formatPercent(summary.hitRate)}
            />
            <Stat
              icon={HistoryIcon}
              label="Last run"
              value={timeAgo(summary.latest?.build_start_time)}
            />
          </div>

          <Card className="gap-4">
            <CardHeader>
              <CardTitle>
                {summary.timed
                  ? "Action time per build"
                  : "Executed actions per build"}
              </CardTitle>
              <CardDescription>
                {summary.timed
                  ? "Wall time of this target's actions"
                  : "Actions of this target the ActionCache could not serve, so they ran"}
                , over the last {runs.length} builds. Click a bar to open its
                build.
              </CardDescription>
            </CardHeader>
            <CardContent>
              {summary.timed ? (
                <RunChart
                  runs={runs}
                  value={(ex) => ex.action_duration_ms}
                  format={formatDuration}
                />
              ) : (
                <RunChart
                  runs={runs}
                  value={(ex) => ex.cache_misses ?? 0}
                  format={(n) => `${n.toLocaleString()} executed`}
                />
              )}
              <div className="mt-3 flex gap-4 text-xs text-muted-foreground">
                {[
                  ["bg-success", "passed"],
                  ["bg-destructive", "failed"],
                  ["bg-muted-foreground/30", "fully cached"],
                ].map(([color, name]) => (
                  <span key={name} className="inline-flex items-center gap-1.5">
                    <span className={cn("size-2 rounded-full", color)} />
                    {name}
                  </span>
                ))}
              </div>
            </CardContent>
          </Card>

          {latestInputs && (
            <TargetInputs
              inputs={latestInputs.heaviest_inputs!}
              invocationId={latestInputs.invocation_id}
            />
          )}

          {summary.latest && (
            <TargetTimeline
              label={label}
              invocationId={summary.latest.invocation_id}
            />
          )}

          <Card className="gap-0 pb-0">
            <CardHeader className="pb-4">
              <CardTitle>Executions</CardTitle>
              <CardDescription>
                One per build, newest first.
                {!summary.counted &&
                  " Per-action counts need --build_event_publish_all_actions; the cache columns come from this server's ActionCache."}
              </CardDescription>
            </CardHeader>
            <CardContent className="px-0">
              <div className="max-h-[32rem] overflow-auto border-t">
                <Table>
                  <TableHeader className="sticky top-0 bg-card">
                    <TableRow>
                      <TableHead className="pl-6">Build</TableHead>
                      <TableHead>Status</TableHead>
                      <TableHead className="text-right">Cache hits</TableHead>
                      <TableHead className="text-right">Executed</TableHead>
                      <TableHead>Executed mnemonics</TableHead>
                      {summary.counted && (
                        <TableHead className="text-right">Failed</TableHead>
                      )}
                      {summary.timed && (
                        <TableHead className="text-right">
                          Action time
                        </TableHead>
                      )}
                      <TableHead className="text-right">Build time</TableHead>
                      <TableHead className="pr-6 text-right">Started</TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {[...history].reverse().map((ex) => (
                      <TableRow key={ex.invocation_id}>
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
                        <TableCell className="text-right tabular-nums">
                          {(ex.cache_hits ?? 0).toLocaleString()}
                        </TableCell>
                        <TableCell
                          className={cn(
                            "text-right tabular-nums",
                            (ex.cache_misses ?? 0) > 0 && "font-semibold",
                          )}
                        >
                          {(ex.cache_misses ?? 0).toLocaleString()}
                        </TableCell>
                        <TableCell className="max-w-xs">
                          <div className="flex flex-wrap gap-1">
                            {Object.entries(ex.missed_mnemonics ?? {})
                              .sort((a, b) => b[1] - a[1])
                              .slice(0, 3)
                              .map(([mnemonic, n]) => (
                                <Badge
                                  key={mnemonic}
                                  variant="secondary"
                                  className="font-mono"
                                >
                                  {mnemonic}
                                  {n > 1 && ` ×${n}`}
                                </Badge>
                              ))}
                          </div>
                        </TableCell>
                        {summary.counted && (
                          <TableCell
                            className={cn(
                              "text-right tabular-nums",
                              ex.failed_actions > 0 && "text-destructive",
                            )}
                          >
                            {ex.failed_actions.toLocaleString()}
                          </TableCell>
                        )}
                        {summary.timed && (
                          <TableCell className="text-right tabular-nums">
                            {formatDuration(ex.action_duration_ms)}
                          </TableCell>
                        )}
                        <TableCell className="text-right text-muted-foreground tabular-nums">
                          {formatDuration(ex.build_duration_ms)}
                        </TableCell>
                        <TableCell className="pr-6 text-right text-muted-foreground">
                          {timeAgo(ex.build_start_time)}
                        </TableCell>
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              </div>
            </CardContent>
          </Card>

          {env.length > 0 && (
            <Card className="gap-0 pb-0">
              <CardHeader className="pb-4">
                <CardTitle>Action environment</CardTitle>
                <CardDescription>
                  As of the latest build; secret values arrive redacted.
                </CardDescription>
              </CardHeader>
              <CardContent className="px-0">
                <div className="max-h-80 overflow-auto border-t">
                  <Table>
                    <TableBody>
                      {env.map(([key, value]) => (
                        <TableRow key={key}>
                          <TableCell className="w-1/3 pl-6 font-mono text-xs">
                            {key}
                          </TableCell>
                          <TableCell className="pr-6 font-mono text-xs break-all whitespace-normal">
                            {value === "<redacted>" ? (
                              <Badge variant="outline" className="gap-1">
                                <LockIcon className="size-3" /> redacted
                              </Badge>
                            ) : (
                              value
                            )}
                          </TableCell>
                        </TableRow>
                      ))}
                    </TableBody>
                  </Table>
                </div>
              </CardContent>
            </Card>
          )}
        </>
      )}
    </div>
  );
}
