import { DatabaseIcon, LaptopIcon, ServerIcon } from "lucide-react";
import { Badge } from "@/shadcn/ui/badge";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/shadcn/ui/card";

type Place = "workers" | "host" | "cache";

/** Where a Bazel runner ran its spawns; `null` for `internal`, which spawns nothing. */
function placeOf(runner: string): Place | null {
  if (runner === "internal") return null;
  if (runner === "remote") return "workers";
  if (runner.endsWith(" cache hit")) return "cache";
  return "host";
}

const PLACES: {
  place: Place;
  title: string;
  detail: string;
  icon: typeof ServerIcon;
  bar: string;
}[] = [
  {
    place: "workers",
    title: "RBE workers",
    detail: "Executed remotely by the FerrisRBE worker pool.",
    icon: ServerIcon,
    bar: "bg-primary",
  },
  {
    place: "host",
    title: "Bazel host",
    detail:
      "Executed where Bazel ran: the CI runner or a laptop (sandboxes, local, persistent workers).",
    icon: LaptopIcon,
    bar: "bg-chart-2",
  },
  {
    place: "cache",
    title: "Cache",
    detail:
      "Not executed: served by the remote cache or skipped by Bazel's own action cache.",
    icon: DatabaseIcon,
    bar: "bg-muted-foreground/40",
  },
];

function share(count: number, total: number) {
  const percent = (count / total) * 100;
  return count > 0 && percent < 1 ? "<1%" : `${Math.round(percent)}%`;
}

function runnerLabel(runner: string) {
  return runner === "worker" ? "persistent worker" : runner;
}

/**
 * The spawns of a build by where they ran, from Bazel's own
 * "N processes: ..." split: RBE workers, the machine that ran Bazel, or a
 * cache. Bazel prints its action cache hits in that line but reports them
 * apart from the runners, so they come in separately.
 */
export default function RunnerBreakdown({
  runners,
  actionCacheHits,
}: {
  runners: Record<string, number>;
  actionCacheHits: number;
}) {
  const counts = Object.entries(runners);
  if (actionCacheHits > 0) counts.push(["action cache hit", actionCacheHits]);
  const groups = PLACES.map((p) => {
    const members = counts
      .filter(([runner]) => placeOf(runner) === p.place)
      .sort((a, b) => b[1] - a[1]);
    return { ...p, members, count: members.reduce((n, [, c]) => n + c, 0) };
  });
  const total = groups.reduce((n, g) => n + g.count, 0);
  const internal = runners.internal ?? 0;
  if (total === 0) return null;

  return (
    <Card className="gap-4">
      <CardHeader>
        <CardTitle>Where actions ran</CardTitle>
        <CardDescription>
          {total.toLocaleString()} spawns, as Bazel counted them
          {internal > 0 &&
            ` (plus ${internal.toLocaleString()} internal, which spawn no process)`}
          .
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="flex h-2.5 overflow-hidden rounded-full bg-muted">
          {groups
            .filter((g) => g.count > 0)
            .map((g) => (
              <div
                key={g.place}
                className={g.bar}
                style={{ width: `max(${(g.count / total) * 100}%, 4px)` }}
                title={`${g.title}: ${g.count.toLocaleString()}`}
              />
            ))}
        </div>
        <div className="grid gap-3 sm:grid-cols-3">
          {groups.map((g) => (
            <div key={g.place} className="rounded-lg border p-3">
              <div className="flex items-center gap-2 text-sm font-medium">
                <span className={`size-2.5 rounded-full ${g.bar}`} />
                <g.icon className="size-4 text-muted-foreground" />
                {g.title}
              </div>
              <div className="mt-1 text-2xl font-semibold tabular-nums">
                {g.count.toLocaleString()}
                <span className="ml-1.5 text-sm font-normal text-muted-foreground">
                  {share(g.count, total)}
                </span>
              </div>
              <p className="mt-1 text-xs text-muted-foreground">{g.detail}</p>
              {g.members.length > 0 && (
                <div className="mt-2 flex flex-wrap gap-1.5">
                  {g.members.map(([runner, count]) => (
                    <Badge
                      key={runner}
                      variant="secondary"
                      className="font-mono text-xs"
                    >
                      {runnerLabel(runner)} · {count.toLocaleString()}
                    </Badge>
                  ))}
                </div>
              )}
            </div>
          ))}
        </div>
      </CardContent>
    </Card>
  );
}
