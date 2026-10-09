import { cn } from "cn";
import { Badge } from "@/shadcn/ui/badge";

type Tone = "success" | "failure" | "running" | "warning" | "info" | "neutral";

const TONES: Record<Tone, string> = {
  success: "border-success/30 bg-success/10 text-success [&>span]:bg-success",
  failure:
    "border-destructive/30 bg-destructive/10 text-destructive [&>span]:bg-destructive",
  running:
    "border-primary/30 bg-primary/10 text-primary [&>span]:bg-primary [&>span]:animate-pulse",
  warning:
    "border-warning/40 bg-warning/15 text-foreground [&>span]:bg-warning",
  info: "border-chart-2/30 bg-chart-2/10 text-chart-2 [&>span]:bg-chart-2",
  neutral:
    "border-border bg-muted text-muted-foreground [&>span]:bg-muted-foreground",
};

const STATUSES: Record<string, { tone: Tone; label: string }> = {
  in_progress: { tone: "running", label: "In progress" },
  success: { tone: "success", label: "Success" },
  failure: { tone: "failure", label: "Failure" },
  interrupted: { tone: "warning", label: "Interrupted" },
  cached: { tone: "info", label: "Cached" },
  dependency: { tone: "neutral", label: "Dependency" },
  idle: { tone: "success", label: "Idle" },
  busy: { tone: "running", label: "Busy" },
  unhealthy: { tone: "failure", label: "Unhealthy" },
  passed: { tone: "success", label: "Passed" },
  failed: { tone: "failure", label: "Failed" },
  flaky: { tone: "warning", label: "Flaky" },
  timeout: { tone: "failure", label: "Timeout" },
  incomplete: { tone: "failure", label: "Incomplete" },
  remote_failure: { tone: "failure", label: "Remote failure" },
  failed_to_build: { tone: "failure", label: "Failed to build" },
  tool_halted_before_testing: { tone: "neutral", label: "Halted" },
  no_status: { tone: "neutral", label: "No status" },
};

export default function StatusBadge({ status }: { status: string }) {
  const known = STATUSES[status.toLowerCase()];
  const tone = known?.tone ?? "neutral";
  return (
    <Badge variant="outline" className={cn("gap-1.5 font-medium", TONES[tone])}>
      <span className="size-1.5 rounded-full" />
      {known?.label ?? status}
    </Badge>
  );
}
