import {
  ActivityIcon,
  CircleAlertIcon,
  CircleCheckIcon,
  HeartPulseIcon,
  ServerIcon,
  ServerOffIcon,
} from "lucide-react";
import { useLiveWorkers } from "@/hooks/useLive";
import StatusBadge from "@/components/StatusBadge";
import { EmptyState, PageHeader, Stat } from "@/components/page";
import type { WorkerView } from "@/types";
import { Badge } from "@/shadcn/ui/badge";
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/shadcn/ui/card";
import { Skeleton } from "@/shadcn/ui/skeleton";

function WorkerCard({ worker }: { worker: WorkerView }) {
  const stale = worker.heartbeat_age_secs > 30;
  return (
    <Card className="gap-4">
      <CardHeader>
        <CardTitle className="flex items-center gap-2 font-mono text-sm">
          <ServerIcon className="size-4 text-muted-foreground" />
          <span className="truncate" title={worker.worker_id}>
            {worker.worker_id}
          </span>
        </CardTitle>
        <CardDescription>{worker.worker_type || "worker"}</CardDescription>
        <CardAction>
          <StatusBadge status={worker.state} />
        </CardAction>
      </CardHeader>
      <CardContent className="space-y-3 text-sm">
        <div className="flex items-center justify-between gap-3">
          <span className="text-muted-foreground">Running</span>
          <span
            className="truncate font-mono text-xs"
            title={worker.current_execution ?? undefined}
          >
            {worker.current_execution ?? "–"}
          </span>
        </div>
        <div className="flex items-center justify-between gap-3">
          <span className="inline-flex items-center gap-1.5 text-muted-foreground">
            <HeartPulseIcon className="size-3.5" /> Heartbeat
          </span>
          <span
            className={
              stale
                ? "font-medium text-destructive tabular-nums"
                : "tabular-nums"
            }
          >
            {worker.heartbeat_age_secs}s ago
          </span>
        </div>
        {worker.labels.length > 0 && (
          <div className="flex flex-wrap gap-1.5 border-t pt-3">
            {worker.labels.map((label) => (
              <Badge key={label} variant="secondary" className="font-mono">
                {label}
              </Badge>
            ))}
          </div>
        )}
      </CardContent>
    </Card>
  );
}

export default function Workers() {
  const workers = useLiveWorkers();
  const count = (state: string) =>
    workers?.filter((w) => w.state.toLowerCase() === state).length ?? 0;

  return (
    <div className="space-y-6">
      <PageHeader
        icon={ServerIcon}
        title="Workers"
        description="Remote execution workers registered with the scheduler, live."
      />

      {workers === null ? (
        <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
          {Array.from({ length: 3 }, (_, i) => (
            <Skeleton key={i} className="h-44 rounded-xl" />
          ))}
        </div>
      ) : workers.length === 0 ? (
        <Card>
          <EmptyState icon={ServerOffIcon} title="No workers connected">
            Remote execution starts once a worker registers through the
            WorkerService.
          </EmptyState>
        </Card>
      ) : (
        <>
          <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
            <Stat icon={ServerIcon} label="Workers" value={workers.length} />
            <Stat icon={CircleCheckIcon} label="Idle" value={count("idle")} />
            <Stat icon={ActivityIcon} label="Busy" value={count("busy")} />
            <Stat
              icon={CircleAlertIcon}
              label="Unhealthy"
              value={count("unhealthy")}
            />
          </div>
          <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
            {workers.map((worker) => (
              <WorkerCard key={worker.worker_id} worker={worker} />
            ))}
          </div>
        </>
      )}
    </div>
  );
}
