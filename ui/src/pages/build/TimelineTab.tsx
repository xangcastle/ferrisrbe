import { useState } from "react";
import { ActivityIcon, GaugeIcon, RouteIcon, TimerIcon } from "lucide-react";
import ProfileGantt, {
  type ColorBy,
  CriticalPathList,
  ExecutorLegend,
  MnemonicLegend,
  useBuildProfile,
  WorkerLegend,
} from "@/components/ProfileGantt";
import { EmptyState, Stat } from "@/components/page";
import { formatDuration } from "@/lib/format";
import type { BuildSummary } from "@/types";
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/shadcn/ui/card";
import { Skeleton } from "@/shadcn/ui/skeleton";
import { Tabs, TabsList, TabsTrigger } from "@/shadcn/ui/tabs";

export default function TimelineTab({ build }: { build: BuildSummary }) {
  const { profile, loading, error } = useBuildProfile(build.invocation_id);
  const [colorBy, setColorBy] = useState<ColorBy>("mnemonic");

  if (loading) return <Skeleton className="h-96 rounded-xl" />;
  if (error) return <p className="text-sm text-destructive">{error}</p>;
  if (!profile)
    return (
      <Card>
        <EmptyState icon={ActivityIcon} title="No profile for this build">
          {build.status === "in_progress"
            ? "Bazel uploads its profile when the build ends."
            : "Bazel uploads its profile to the remote cache when it has one; it is gone once the cache evicts it."}
        </EmptyState>
      </Card>
    );

  const criticalMs = profile.critical_path.reduce(
    (s, c) => s + c.duration_ms,
    0,
  );
  const actionMs = profile.actions.reduce((s, a) => s + a.duration_ms, 0);

  return (
    <div className="space-y-4">
      <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
        <Stat
          icon={TimerIcon}
          label="Wall time"
          value={formatDuration(Math.round(profile.total_ms))}
        />
        <Stat
          icon={RouteIcon}
          label="Critical path"
          value={`${formatDuration(Math.round(criticalMs))} · ${Math.round((criticalMs / profile.total_ms) * 100)}%`}
        />
        <Stat
          icon={GaugeIcon}
          label="Average parallelism"
          value={`${(actionMs / profile.total_ms).toFixed(1)}×`}
        />
        <Stat
          icon={ActivityIcon}
          label="Actions drawn"
          value={profile.actions.length.toLocaleString()}
        />
      </div>

      <Card className="gap-4">
        <CardHeader>
          <CardTitle>Actions over time</CardTitle>
          <CardDescription>
            One row per Bazel thread; the critical path is orange, the rest is
            colored by mnemonic, by where it ran or by the RBE worker pod that
            ran it, and dashed lines mark the build phases. Click an action to
            open its target.
            {profile.omitted_actions > 0 &&
              ` ${profile.omitted_actions.toLocaleString()} shorter actions are left out.`}
          </CardDescription>
          {profile.executors && (
            <CardAction>
              <Tabs
                value={colorBy}
                onValueChange={(v) => setColorBy(v as ColorBy)}
              >
                <TabsList>
                  <TabsTrigger value="mnemonic">Mnemonic</TabsTrigger>
                  <TabsTrigger value="executor">Where it ran</TabsTrigger>
                  {Object.keys(profile.workers ?? {}).length > 0 && (
                    <TabsTrigger value="worker">Worker</TabsTrigger>
                  )}
                </TabsList>
              </Tabs>
            </CardAction>
          )}
        </CardHeader>
        <CardContent className="space-y-3">
          <ProfileGantt
            profile={profile}
            actions={profile.actions}
            colorBy={colorBy}
          />
          {colorBy === "executor" ? (
            <ExecutorLegend profile={profile} />
          ) : colorBy === "worker" ? (
            <WorkerLegend profile={profile} />
          ) : (
            <MnemonicLegend actions={profile.actions} />
          )}
        </CardContent>
      </Card>

      <Card className="gap-4">
        <CardHeader>
          <CardTitle>Critical path</CardTitle>
          <CardDescription>
            The chain of actions the build had to wait for, in order. Making any
            other action faster does not shorten the build.
          </CardDescription>
        </CardHeader>
        <CardContent>
          <CriticalPathList profile={profile} />
        </CardContent>
      </Card>
    </div>
  );
}
