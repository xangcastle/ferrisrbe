import { useEffect, useState } from "react";
import { Link, useParams, useSearchParams } from "react-router-dom";
import { ArrowLeftIcon, SearchXIcon } from "lucide-react";
import { useLiveBuild } from "@/hooks/useLive";
import StatusBadge from "@/components/StatusBadge";
import { BuildOrigin, invocationTitle } from "@/components/BuildMeta";
import {
  cacheHitRatio,
  formatDuration,
  formatElapsed,
  formatPercent,
} from "@/lib/format";
import type { BuildSummary } from "@/types";
import { Button } from "@/shadcn/ui/button";
import { Card, CardContent } from "@/shadcn/ui/card";
import { Skeleton } from "@/shadcn/ui/skeleton";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/shadcn/ui/tabs";
import InputsTab, { useBuildInputs } from "./build/InputsTab";
import OutputTab from "./build/OutputTab";
import OverviewTab from "./build/OverviewTab";
import TargetsTab from "./build/TargetsTab";
import TimelineTab from "./build/TimelineTab";

const TABS = ["overview", "inputs", "targets", "timeline", "output"] as const;
type Tab = (typeof TABS)[number];

function useNow(active: boolean) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active) return;
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [active]);
  return now;
}

function Metric({
  label,
  value,
  tone,
}: {
  label: string;
  value: string;
  tone?: "destructive";
}) {
  return (
    <div className="min-w-0 px-5 py-3">
      <p className="text-xs text-muted-foreground">{label}</p>
      <p
        className={
          tone === "destructive"
            ? "text-lg font-semibold text-destructive tabular-nums"
            : "text-lg font-semibold tabular-nums"
        }
      >
        {value}
      </p>
    </div>
  );
}

function Header({
  build,
  targetCount,
}: {
  build: BuildSummary;
  targetCount: number;
}) {
  const live = build.status === "in_progress";
  const now = useNow(live);
  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div className="min-w-0 space-y-1">
          <div className="flex items-center gap-3">
            <Button variant="ghost" size="icon" className="-ml-2" asChild>
              <Link to="/builds" aria-label="Back to builds">
                <ArrowLeftIcon />
              </Link>
            </Button>
            <StatusBadge status={build.status} />
            <h2 className="truncate text-xl font-semibold">
              {invocationTitle(build)}
            </h2>
          </div>
          <p className="truncate pl-10 text-sm text-muted-foreground">
            <BuildOrigin build={build} />{" "}
            <span className="font-mono text-xs">{build.invocation_id}</span>
          </p>
        </div>
        <Button variant="outline" size="sm" asChild>
          <Link to={`/misses/${encodeURIComponent(build.invocation_id)}`}>
            <SearchXIcon /> Cache misses
          </Link>
        </Button>
      </div>
      <Card className="py-0">
        <CardContent className="grid grid-cols-2 divide-x divide-y px-0 sm:grid-cols-5 sm:divide-y-0">
          <Metric
            label={live ? "Running for" : "Duration"}
            value={
              live
                ? formatElapsed(build.start_time, now)
                : formatDuration(build.duration_ms)
            }
          />
          <Metric
            label="Actions"
            value={build.total_actions.toLocaleString()}
          />
          <Metric
            label="Cache hits"
            value={formatPercent(cacheHitRatio(build))}
          />
          <Metric
            label="Failed actions"
            value={build.failed_actions.toLocaleString()}
            tone={build.failed_actions > 0 ? "destructive" : undefined}
          />
          <Metric label="Targets" value={targetCount.toLocaleString()} />
        </CardContent>
      </Card>
    </div>
  );
}

export default function BuildDetail() {
  const { id = "" } = useParams<{ id: string }>();
  const { build, targets, failures, log, loading, error } = useLiveBuild(id);
  const { inputs, error: inputsError } = useBuildInputs(id);
  const [params, setParams] = useSearchParams();

  if (loading)
    return (
      <div className="space-y-4">
        <Skeleton className="h-10 w-2/3" />
        <Skeleton className="h-20 rounded-xl" />
        <Skeleton className="h-96 rounded-xl" />
      </div>
    );
  if (error) return <p className="text-sm text-destructive">{error}</p>;
  if (!build)
    return <p className="text-sm text-muted-foreground">Build not found.</p>;

  const requested = params.get("tab") as Tab | null;
  const tab: Tab =
    requested && TABS.includes(requested)
      ? requested
      : build.status === "in_progress"
        ? "output"
        : "overview";

  return (
    <div className="space-y-6">
      <Header
        build={build}
        targetCount={Math.max(targets.length, build.target_count ?? 0)}
      />
      <Tabs
        value={tab}
        onValueChange={(next) =>
          setParams(
            (p) => {
              p.set("tab", next);
              return p;
            },
            { replace: true },
          )
        }
        className="gap-4"
      >
        <TabsList>
          <TabsTrigger value="overview">Overview</TabsTrigger>
          <TabsTrigger value="inputs">Inputs</TabsTrigger>
          <TabsTrigger value="targets">
            Targets
            <span className="text-muted-foreground tabular-nums">
              {targets.length}
            </span>
          </TabsTrigger>
          <TabsTrigger value="timeline">Timeline</TabsTrigger>
          <TabsTrigger value="output">Output</TabsTrigger>
        </TabsList>
        <TabsContent value="overview">
          <OverviewTab build={build} inputs={inputs} />
        </TabsContent>
        <TabsContent value="inputs">
          <InputsTab build={build} inputs={inputs} error={inputsError} />
        </TabsContent>
        <TabsContent value="targets">
          <TargetsTab build={build} targets={targets} />
        </TabsContent>
        <TabsContent value="timeline">
          <TimelineTab build={build} />
        </TabsContent>
        <TabsContent value="output">
          <OutputTab build={build} log={log} failures={failures} />
        </TabsContent>
      </Tabs>
    </div>
  );
}
