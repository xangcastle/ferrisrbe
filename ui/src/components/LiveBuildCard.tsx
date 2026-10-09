import { useEffect, useRef, useState } from "react";
import { Link } from "react-router-dom";
import { TimerIcon } from "lucide-react";
import { useLiveLogTail } from "@/hooks/useLive";
import { formatElapsed } from "@/lib/format";
import type { BuildSummary } from "@/types";
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/shadcn/ui/card";
import StatusBadge from "./StatusBadge";
import { BuildOrigin, invocationTitle } from "./BuildMeta";

/**
 * One running build: live elapsed time and a tailing, auto-scrolling log.
 * `build` comes from the dashboard's live list, so the card renders at
 * once; only the console tail is fetched, never the build's events.
 */
export default function LiveBuildCard({ build }: { build: BuildSummary }) {
  const log = useLiveLogTail(build.invocation_id);
  const [now, setNow] = useState(() => Date.now());
  const logRef = useRef<HTMLPreElement>(null);

  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);

  useEffect(() => {
    const el = logRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [log]);

  return (
    <Card className="gap-0 overflow-hidden py-0">
      <CardHeader className="border-b py-4">
        <CardTitle className="truncate">
          <Link
            to={`/builds/${encodeURIComponent(build.invocation_id)}`}
            className="hover:text-primary"
          >
            {invocationTitle(build)}
          </Link>
        </CardTitle>
        <CardDescription className="truncate">
          <BuildOrigin build={build} />
          {!build.metadata?.USER && build.invocation_id}
        </CardDescription>
        <CardAction className="flex items-center gap-3">
          <span className="inline-flex items-center gap-1 font-mono text-sm text-muted-foreground tabular-nums">
            <TimerIcon className="size-3.5" />
            {formatElapsed(build.start_time, now)}
          </span>
          <StatusBadge status={build.status} />
        </CardAction>
      </CardHeader>
      <div className="flex gap-4 border-b bg-muted/40 px-6 py-2 text-xs text-muted-foreground tabular-nums">
        <span>{build.total_actions.toLocaleString()} actions</span>
        <span>{build.cached_actions.toLocaleString()} cached</span>
        {build.failed_actions > 0 && (
          <span className="font-medium text-destructive">
            {build.failed_actions.toLocaleString()} failed
          </span>
        )}
      </div>
      <CardContent className="p-0">
        <pre
          ref={logRef}
          className="h-56 overflow-auto bg-neutral-950 p-4 font-mono text-xs leading-5 text-neutral-200"
        >
          {log || "Waiting for output…"}
        </pre>
      </CardContent>
    </Card>
  );
}
