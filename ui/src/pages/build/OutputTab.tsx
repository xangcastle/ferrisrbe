import { useEffect, useRef, useState } from "react";
import { ChevronRightIcon } from "lucide-react";
import { cn } from "cn";
import { blobUrl, getBlobText } from "@/api/bes";
import type { BuildSummary, FailedAction, FileReference } from "@/types";
import { Badge } from "@/shadcn/ui/badge";
import { Button } from "@/shadcn/ui/button";
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/shadcn/ui/card";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/shadcn/ui/tabs";

function LogPanel({ log, live }: { log: string; live: boolean }) {
  const ref = useRef<HTMLPreElement>(null);
  const [follow, setFollow] = useState(true);
  useEffect(() => {
    const el = ref.current;
    if (el && live && follow) el.scrollTop = el.scrollHeight;
  }, [log, live, follow]);
  return (
    <Card className="gap-0 overflow-hidden py-0">
      <CardHeader className="border-b py-3">
        <CardTitle className="text-sm">Console</CardTitle>
        <CardDescription>
          Bazel's output as a terminal shows it.
        </CardDescription>
        {live && (
          <CardAction>
            <Button
              variant={follow ? "secondary" : "outline"}
              size="sm"
              onClick={() => setFollow((f) => !f)}
            >
              {follow ? "Following" : "Follow"}
            </Button>
          </CardAction>
        )}
      </CardHeader>
      <pre
        ref={ref}
        onWheel={(e) => e.deltaY < 0 && setFollow(false)}
        className="h-[36rem] overflow-auto bg-neutral-950 p-4 font-mono text-xs leading-5 text-neutral-200"
      >
        {log || (live ? "Waiting for output…" : "No console output recorded.")}
      </pre>
    </Card>
  );
}

function ActionOutput({
  label,
  file,
}: {
  label: string;
  file?: FileReference | null;
}) {
  const [text, setText] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const url = blobUrl(file);
  if (!url) return null;
  return (
    <div>
      <Button
        variant="ghost"
        size="sm"
        className="-ml-2 h-7"
        onClick={async () => {
          if (text !== null) {
            setText(null);
            return;
          }
          setLoading(true);
          try {
            setText(await getBlobText(url));
          } catch {
            setText("(blob not available in the CAS)");
          } finally {
            setLoading(false);
          }
        }}
      >
        <ChevronRightIcon
          className={cn("transition-transform", text !== null && "rotate-90")}
        />
        {loading ? "Loading…" : label}
      </Button>
      {text !== null && (
        <pre className="mt-1 max-h-96 overflow-auto rounded-lg bg-neutral-950 p-3 font-mono text-xs text-neutral-200">
          {text || "(empty)"}
        </pre>
      )}
    </div>
  );
}

function FailuresPanel({ failures }: { failures: FailedAction[] }) {
  if (failures.length === 0)
    return (
      <Card>
        <CardContent className="text-sm text-muted-foreground">
          No failed actions.
        </CardContent>
      </Card>
    );
  return (
    <div className="space-y-3">
      {failures.map((action, i) => (
        <Card key={`${action.label}-${i}`} className="gap-2 py-4">
          <CardHeader className="px-4">
            <CardTitle className="font-mono text-sm break-all">
              {action.label}
            </CardTitle>
            <CardDescription className="flex flex-wrap items-center gap-2">
              <Badge variant="secondary">{action.type}</Badge>
              {action.exit_code !== undefined && action.exit_code !== null && (
                <Badge variant="destructive">exit {action.exit_code}</Badge>
              )}
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-1 px-4">
            <ActionOutput label="stderr" file={action.stderr} />
            <ActionOutput label="stdout" file={action.stdout} />
          </CardContent>
        </Card>
      ))}
    </div>
  );
}

export default function OutputTab({
  build,
  log,
  failures,
}: {
  build: BuildSummary;
  log: string;
  failures: FailedAction[];
}) {
  const live = build.status === "in_progress";
  return (
    <Tabs
      defaultValue={failures.length > 0 && !live ? "failures" : "log"}
      className="gap-4"
    >
      <TabsList>
        <TabsTrigger value="log">Log</TabsTrigger>
        <TabsTrigger value="failures">
          Failed actions
          <span
            className={cn(
              "tabular-nums",
              failures.length > 0
                ? "text-destructive"
                : "text-muted-foreground",
            )}
          >
            {failures.length}
          </span>
        </TabsTrigger>
      </TabsList>
      <TabsContent value="log">
        <LogPanel log={log} live={live} />
      </TabsContent>
      <TabsContent value="failures">
        <FailuresPanel failures={failures} />
      </TabsContent>
    </Tabs>
  );
}
