import { Link, useNavigate } from "react-router-dom";
import {
  ActivityIcon,
  CheckCircle2Icon,
  CpuIcon,
  HammerIcon,
  type LucideIcon,
} from "lucide-react";
import { useLiveBuilds } from "@/hooks/useLive";
import LiveBuildCard from "@/components/LiveBuildCard";
import StatusBadge from "@/components/StatusBadge";
import { BuildOrigin, invocationTitle } from "@/components/BuildMeta";
import {
  cacheHitRatio,
  formatDuration,
  formatPercent,
  timeAgo,
} from "@/lib/format";
import type { GlobalStats } from "@/types";
import { Button } from "@/shadcn/ui/button";
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/shadcn/ui/card";
import { Progress } from "@/shadcn/ui/progress";
import { Skeleton } from "@/shadcn/ui/skeleton";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/shadcn/ui/table";

const RECENT = 8;

function StatCard({
  title,
  value,
  icon: Icon,
  children,
}: {
  title: string;
  value: string;
  icon: LucideIcon;
  children?: React.ReactNode;
}) {
  return (
    <Card className="gap-3">
      <CardHeader>
        <CardDescription>{title}</CardDescription>
        <CardTitle className="text-3xl font-semibold tabular-nums">
          {value}
        </CardTitle>
        <CardAction>
          <span className="flex size-9 items-center justify-center rounded-lg bg-primary/10 text-primary">
            <Icon className="size-4" />
          </span>
        </CardAction>
      </CardHeader>
      {children && (
        <CardContent className="text-sm text-muted-foreground">
          {children}
        </CardContent>
      )}
    </Card>
  );
}

function StatCards({ stats }: { stats: GlobalStats }) {
  const finished =
    stats.successful_builds +
    stats.failed_builds +
    (stats.interrupted_builds ?? 0);
  const successRate =
    finished === 0 ? null : stats.successful_builds / finished;
  return (
    <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 xl:grid-cols-4">
      <StatCard
        title="Builds"
        value={stats.total_builds.toLocaleString()}
        icon={HammerIcon}
      >
        {stats.successful_builds.toLocaleString()} passed ·{" "}
        {stats.failed_builds.toLocaleString()} failed
        {stats.interrupted_builds
          ? ` · ${stats.interrupted_builds.toLocaleString()} interrupted`
          : ""}
      </StatCard>
      <StatCard
        title="Success rate"
        value={formatPercent(successRate)}
        icon={CheckCircle2Icon}
      >
        <Progress value={(successRate ?? 0) * 100} className="h-1.5" />
      </StatCard>
      <StatCard
        title="Running now"
        value={stats.in_progress_builds.toLocaleString()}
        icon={ActivityIcon}
      >
        {stats.in_progress_builds === 0 ? "Idle" : "Streaming live from Bazel"}
      </StatCard>
      <StatCard
        title="Actions"
        value={stats.total_actions.toLocaleString()}
        icon={CpuIcon}
      >
        {stats.failed_actions.toLocaleString()} failed
      </StatCard>
    </div>
  );
}

function DashboardSkeleton() {
  return (
    <div className="space-y-6">
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 xl:grid-cols-4">
        {Array.from({ length: 4 }, (_, i) => (
          <Skeleton key={i} className="h-32 rounded-xl" />
        ))}
      </div>
      <Skeleton className="h-72 rounded-xl" />
    </div>
  );
}

export default function Dashboard() {
  const { builds, stats, loading, error } = useLiveBuilds(50);
  const navigate = useNavigate();
  const inProgress = builds.filter((b) => b.status === "in_progress");
  const recent = builds
    .filter((b) => b.status !== "in_progress")
    .slice(0, RECENT);

  if (loading) return <DashboardSkeleton />;
  if (error)
    return (
      <Card className="border-destructive/40">
        <CardContent className="text-destructive">{error}</CardContent>
      </Card>
    );

  return (
    <div className="space-y-6">
      {stats && <StatCards stats={stats} />}

      <section className="space-y-3">
        <div className="flex items-center gap-2">
          <h2 className="text-lg font-semibold">Running now</h2>
          {inProgress.length > 0 && (
            <span className="rounded-full bg-primary/10 px-2 py-0.5 text-xs font-medium text-primary tabular-nums">
              {inProgress.length}
            </span>
          )}
        </div>
        {inProgress.length === 0 ? (
          <Card className="border-dashed">
            <CardContent className="flex flex-col items-center gap-1 py-6 text-center">
              <ActivityIcon className="size-5 text-muted-foreground" />
              <p className="font-medium">No builds in progress</p>
              <p className="text-sm text-muted-foreground">
                Builds that stream to this server's BES show up here live.
              </p>
            </CardContent>
          </Card>
        ) : (
          <div className="grid grid-cols-1 gap-4 2xl:grid-cols-2">
            {inProgress.map((build) => (
              <LiveBuildCard key={build.invocation_id} build={build} />
            ))}
          </div>
        )}
      </section>

      <Card className="gap-0 pb-0">
        <CardHeader className="pb-4">
          <CardTitle>Recent builds</CardTitle>
          <CardDescription>The last finished invocations.</CardDescription>
          <CardAction>
            <Button variant="outline" size="sm" asChild>
              <Link to="/builds">View all</Link>
            </Button>
          </CardAction>
        </CardHeader>
        <CardContent className="px-0">
          {recent.length === 0 ? (
            <p className="px-6 pb-6 text-sm text-muted-foreground">
              No finished builds yet.
            </p>
          ) : (
            <Table>
              <TableHeader className="bg-muted/40">
                <TableRow>
                  <TableHead className="pl-6">Status</TableHead>
                  <TableHead>Invocation</TableHead>
                  <TableHead className="text-right">Duration</TableHead>
                  <TableHead className="text-right">Actions</TableHead>
                  <TableHead className="text-right">Cache hits</TableHead>
                  <TableHead className="pr-6 text-right">Finished</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {recent.map((build) => (
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
                    <TableCell className="max-w-md">
                      <div className="truncate font-medium">
                        {invocationTitle(build)}
                      </div>
                      <div className="truncate text-xs text-muted-foreground">
                        <BuildOrigin build={build} />
                        {!build.metadata?.USER && build.invocation_id}
                      </div>
                    </TableCell>
                    <TableCell className="text-right tabular-nums">
                      {formatDuration(build.duration_ms)}
                    </TableCell>
                    <TableCell className="text-right tabular-nums">
                      {build.total_actions.toLocaleString()}
                    </TableCell>
                    <TableCell className="text-right tabular-nums">
                      {formatPercent(cacheHitRatio(build))}
                    </TableCell>
                    <TableCell className="pr-6 text-right text-muted-foreground">
                      {timeAgo(build.end_time ?? build.start_time)}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
