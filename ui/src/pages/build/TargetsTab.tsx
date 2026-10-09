import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { SearchIcon } from "lucide-react";
import StatusBadge from "@/components/StatusBadge";
import type { BuildSummary, TargetExecution } from "@/types";
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/shadcn/ui/card";
import { Input } from "@/shadcn/ui/input";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/shadcn/ui/table";
import { Tabs, TabsList, TabsTrigger } from "@/shadcn/ui/tabs";

type Bucket = "executed" | "cached" | "failed" | "all";

const BUCKETS: { key: Bucket; label: string }[] = [
  { key: "executed", label: "Executed" },
  { key: "cached", label: "Cached" },
  { key: "failed", label: "Failed" },
  { key: "all", label: "All" },
];

/** A target ran if the ActionCache missed at least one of its actions. */
function bucketTargets(targets: TargetExecution[]) {
  const failed = targets.filter((t) => t.status === "failure");
  const executed = targets.filter(
    (t) => t.status !== "failure" && (t.cache_misses ?? 0) > 0,
  );
  const cached = targets.filter(
    (t) => t.status !== "failure" && (t.cache_misses ?? 0) === 0,
  );
  return { executed, cached, failed, all: targets };
}

function mnemonicSummary(t: TargetExecution) {
  return Object.entries(t.missed_mnemonics ?? {})
    .sort((a, b) => b[1] - a[1])
    .map(([m, n]) => (n > 1 ? `${m}×${n}` : m))
    .join(", ");
}

export default function TargetsTab({
  build,
  targets,
}: {
  build: BuildSummary;
  targets: TargetExecution[];
}) {
  const buckets = useMemo(() => bucketTargets(targets), [targets]);
  const [bucket, setBucket] = useState<Bucket>(
    buckets.failed.length > 0 ? "failed" : "executed",
  );
  const [query, setQuery] = useState("");
  const visible = useMemo(() => {
    const q = query.trim().toLowerCase();
    return buckets[bucket].filter(
      (t) =>
        !q ||
        t.label.toLowerCase().includes(q) ||
        t.target_kind.toLowerCase().includes(q) ||
        Object.keys(t.missed_mnemonics ?? {}).some((m) =>
          m.toLowerCase().includes(q),
        ),
    );
  }, [buckets, bucket, query]);

  return (
    <Card className="gap-0 pb-0">
      <CardHeader className="gap-3 pb-4">
        <CardTitle>Targets</CardTitle>
        <CardDescription>
          {build.status === "in_progress"
            ? "Targets complete as Bazel reports them; cache detail arrives when the build ends."
            : "A target counts as executed when the ActionCache missed one of its actions."}
        </CardDescription>
        <CardAction>
          <div className="relative w-full sm:w-72">
            <SearchIcon className="pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground" />
            <Input
              type="search"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder="Label, kind or mnemonic"
              className="pl-8"
            />
          </div>
        </CardAction>
        <Tabs value={bucket} onValueChange={(v) => setBucket(v as Bucket)}>
          <TabsList>
            {BUCKETS.map(({ key, label }) => (
              <TabsTrigger key={key} value={key}>
                {label}
                <span className="text-muted-foreground tabular-nums">
                  {buckets[key].length}
                </span>
              </TabsTrigger>
            ))}
          </TabsList>
        </Tabs>
      </CardHeader>
      <CardContent className="px-0">
        {targets.length === 0 ? (
          <p className="border-t px-6 py-6 text-sm text-muted-foreground">
            No target data yet. Pass{" "}
            <code className="rounded bg-muted px-1 py-0.5 text-xs">
              --build_event_publish_all_actions
            </code>{" "}
            for per-action detail.
          </p>
        ) : visible.length === 0 ? (
          <p className="border-t px-6 py-6 text-sm text-muted-foreground">
            No targets here.
          </p>
        ) : (
          <div className="max-h-[36rem] overflow-auto border-t">
            <Table>
              <TableHeader className="sticky top-0 bg-card">
                <TableRow>
                  <TableHead className="pl-6">Label</TableHead>
                  <TableHead>Kind</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead className="text-right">Cache</TableHead>
                  <TableHead className="pr-6">Executed actions</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {visible.map((target) => (
                  <TableRow key={target.label}>
                    <TableCell className="max-w-xl pl-6">
                      <Link
                        to={`/targets/${encodeURIComponent(target.label)}`}
                        className="block truncate font-mono text-xs hover:text-primary"
                        title={target.label}
                      >
                        {target.label}
                      </Link>
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      {target.target_kind || "–"}
                    </TableCell>
                    <TableCell>
                      <StatusBadge status={target.status} />
                    </TableCell>
                    <TableCell className="text-right text-xs tabular-nums">
                      {target.cache_hits ?? 0} hit · {target.cache_misses ?? 0}{" "}
                      miss
                    </TableCell>
                    <TableCell className="max-w-sm truncate pr-6 font-mono text-xs text-muted-foreground">
                      {mnemonicSummary(target) || "–"}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
        )}
      </CardContent>
    </Card>
  );
}
