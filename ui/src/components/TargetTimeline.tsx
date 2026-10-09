import { Link } from "react-router-dom";
import { RouteIcon } from "lucide-react";
import ProfileGantt, {
  MnemonicLegend,
  useBuildProfile,
} from "@/components/ProfileGantt";
import { formatDuration } from "@/lib/format";
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
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/shadcn/ui/table";

/**
 * The target's actions in one build's profile: when they ran, how long,
 * and which of them the build waited for.
 */
export default function TargetTimeline({
  label,
  invocationId,
}: {
  label: string;
  invocationId: string;
}) {
  const { profile, loading } = useBuildProfile(invocationId);
  const buildLink = (
    <Link
      to={`/builds/${encodeURIComponent(invocationId)}?tab=timeline`}
      className="font-mono text-xs hover:text-primary"
    >
      {invocationId.slice(0, 13)}…
    </Link>
  );

  if (loading) return <Skeleton className="h-48 rounded-xl" />;

  const mine = profile?.actions.filter((a) => a.target === label) ?? [];
  const hasLabels = profile?.actions.some((a) => a.target) ?? false;

  return (
    <Card className="gap-4">
      <CardHeader>
        <CardTitle>In its latest build</CardTitle>
        <CardDescription>
          {!profile
            ? "That build has no profile: Bazel uploads it to the remote cache, which may have evicted it."
            : !hasLabels
              ? "That build's profile does not name targets; builds with --experimental_profile_include_target_label do."
              : mine.length === 0
                ? "None of its actions ran in that build: everything came from cache."
                : `${mine.length} ${mine.length === 1 ? "action" : "actions"} ran for this target; critical-path ones are orange.`}
        </CardDescription>
        <CardAction>{buildLink}</CardAction>
      </CardHeader>
      {profile && mine.length > 0 && (
        <CardContent className="space-y-4">
          <ProfileGantt
            profile={profile}
            actions={mine}
            from={Math.min(...mine.map((a) => a.start_ms))}
            to={Math.max(...mine.map((a) => a.start_ms + a.duration_ms))}
            rowHeight={16}
            maxHeight="16rem"
          />
          <MnemonicLegend actions={mine} />
          <div className="max-h-72 overflow-auto rounded-lg border">
            <Table>
              <TableHeader className="sticky top-0 bg-card">
                <TableRow>
                  <TableHead className="pl-4">Action</TableHead>
                  <TableHead>Mnemonic</TableHead>
                  <TableHead className="text-right">Started at</TableHead>
                  <TableHead className="pr-4 text-right">Duration</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {[...mine]
                  .sort((a, b) => b.duration_ms - a.duration_ms)
                  .map((a, i) => (
                    <TableRow key={i}>
                      <TableCell className="max-w-xl pl-4">
                        <div className="flex items-center gap-2">
                          {a.critical && (
                            <Badge className="gap-1">
                              <RouteIcon className="size-3" /> critical
                            </Badge>
                          )}
                          <span className="truncate text-xs" title={a.name}>
                            {a.name}
                          </span>
                        </div>
                      </TableCell>
                      <TableCell>
                        <Badge variant="secondary" className="font-mono">
                          {a.mnemonic}
                        </Badge>
                      </TableCell>
                      <TableCell className="text-right text-muted-foreground tabular-nums">
                        {formatDuration(Math.round(a.start_ms))}
                      </TableCell>
                      <TableCell className="pr-4 text-right font-semibold tabular-nums">
                        {formatDuration(Math.round(a.duration_ms))}
                      </TableCell>
                    </TableRow>
                  ))}
              </TableBody>
            </Table>
          </div>
        </CardContent>
      )}
    </Card>
  );
}
