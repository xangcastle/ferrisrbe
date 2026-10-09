import { useMemo } from "react";
import { AlertTriangleIcon } from "lucide-react";
import { BuildOriginCard } from "@/components/BuildMeta";
import RunnerBreakdown from "@/components/RunnerBreakdown";
import type { BuildInputs, BuildSummary } from "@/types";
import { Badge } from "@/shadcn/ui/badge";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/shadcn/ui/card";

function formatTime(iso?: string | null) {
  return iso ? new Date(iso).toLocaleString() : "–";
}

function Field({
  label,
  children,
}: {
  label: string;
  children: React.ReactNode;
}) {
  return (
    <div className="min-w-0">
      <dt className="text-xs font-medium text-muted-foreground">{label}</dt>
      <dd className="mt-1 text-sm break-all">{children}</dd>
    </div>
  );
}

/** Identical messages once, with how many times Bazel reported them. */
function groupErrors(errors: string[]) {
  const counts = new Map<string, number>();
  for (const e of errors) counts.set(e, (counts.get(e) ?? 0) + 1);
  return [...counts.entries()].sort((a, b) => b[1] - a[1]);
}

export default function OverviewTab({
  build,
  inputs,
}: {
  build: BuildSummary;
  inputs: BuildInputs | null;
}) {
  const errors = useMemo(() => groupErrors(build.errors), [build.errors]);
  const status = Object.entries(inputs?.workspace_status ?? {});
  return (
    <div className="space-y-4">
      {errors.length > 0 && (
        <Card className="gap-4 border-destructive/40 bg-destructive/5">
          <CardHeader>
            <CardTitle className="flex items-center gap-2 text-destructive">
              <AlertTriangleIcon className="size-4" />
              Errors
            </CardTitle>
            <CardDescription>
              {build.errors.length} reported, {errors.length} distinct.
            </CardDescription>
          </CardHeader>
          <CardContent>
            <ul className="divide-y divide-destructive/15 rounded-lg border border-destructive/20 bg-card">
              {errors.map(([message, count]) => (
                <li
                  key={message}
                  className="flex items-start justify-between gap-4 px-4 py-2.5"
                >
                  <span className="font-mono text-xs leading-5 break-all">
                    {message}
                  </span>
                  {count > 1 && (
                    <Badge variant="destructive" className="shrink-0">
                      ×{count}
                    </Badge>
                  )}
                </li>
              ))}
            </ul>
          </CardContent>
        </Card>
      )}

      <div className="grid gap-4 xl:grid-cols-2">
        <Card className="gap-4">
          <CardHeader>
            <CardTitle>Metadata</CardTitle>
            <CardDescription>
              From --build_metadata, as CI attaches it.
            </CardDescription>
          </CardHeader>
          <CardContent>
            <BuildOriginCard build={build} />
          </CardContent>
        </Card>

        <Card className="gap-4">
          <CardHeader>
            <CardTitle>Invocation</CardTitle>
          </CardHeader>
          <CardContent>
            <dl className="grid grid-cols-1 gap-4 sm:grid-cols-2">
              <Field label="Invocation ID">
                <span className="font-mono text-xs">{build.invocation_id}</span>
              </Field>
              <Field label="Command">{build.command || "–"}</Field>
              <Field label="Started">{formatTime(build.start_time)}</Field>
              <Field label="Finished">{formatTime(build.end_time)}</Field>
              <Field label="Workspace">
                <span className="font-mono text-xs">
                  {build.workspace || "–"}
                </span>
              </Field>
              <Field label="Actions">
                {build.remote_cache_hits.toLocaleString()} remote cache hits ·{" "}
                {build.local_actions.toLocaleString()} executed ·{" "}
                {build.action_cache_hits.toLocaleString()} skipped by the action
                cache
              </Field>
            </dl>
          </CardContent>
        </Card>
      </div>

      {build.runners && (
        <RunnerBreakdown
          runners={build.runners}
          actionCacheHits={build.action_cache_hits}
        />
      )}

      {status.length > 0 && (
        <Card className="gap-4">
          <CardHeader>
            <CardTitle>Workspace status</CardTitle>
            <CardDescription>
              What --workspace_status_command reported for this build.
            </CardDescription>
          </CardHeader>
          <CardContent>
            <dl className="grid grid-cols-1 gap-4 sm:grid-cols-2 xl:grid-cols-3">
              {status.map(([key, value]) => (
                <Field key={key} label={key}>
                  <span className="font-mono text-xs">{value || "–"}</span>
                </Field>
              ))}
            </dl>
          </CardContent>
        </Card>
      )}
    </div>
  );
}
