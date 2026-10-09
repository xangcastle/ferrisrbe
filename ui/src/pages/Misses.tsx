import { useEffect, useMemo, useState } from "react";
import { Link, useParams } from "react-router-dom";
import {
  ArrowLeftIcon,
  CircleCheckIcon,
  CpuIcon,
  SearchXIcon,
  TargetIcon,
} from "lucide-react";
import { getMisses } from "@/api/bes";
import { EmptyState, PageHeader, SearchInput, Stat } from "@/components/page";
import { formatPercent } from "@/lib/format";
import type { TargetCacheMiss } from "@/types";
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
import { Skeleton } from "@/shadcn/ui/skeleton";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/shadcn/ui/table";

const TOP_MNEMONICS = 8;

/** Executed actions per mnemonic, most first, as horizontal bars. */
function MnemonicBreakdown({ misses }: { misses: TargetCacheMiss[] }) {
  const rows = useMemo(() => {
    const totals = new Map<string, number>();
    for (const m of misses)
      totals.set(m.mnemonic, (totals.get(m.mnemonic) ?? 0) + m.misses);
    return [...totals.entries()]
      .sort((a, b) => b[1] - a[1])
      .slice(0, TOP_MNEMONICS);
  }, [misses]);
  const max = rows[0]?.[1] ?? 1;
  return (
    <div className="space-y-2.5">
      {rows.map(([mnemonic, count]) => (
        <div key={mnemonic} className="flex items-center gap-3 text-sm">
          <span className="w-40 shrink-0 truncate font-mono text-xs">
            {mnemonic}
          </span>
          <div className="h-2 flex-1 overflow-hidden rounded-full bg-muted">
            <div
              className="h-full rounded-full bg-primary"
              style={{ width: `${(count / max) * 100}%` }}
            />
          </div>
          <span className="w-14 shrink-0 text-right tabular-nums">
            {count.toLocaleString()}
          </span>
        </div>
      ))}
    </div>
  );
}

export default function Misses() {
  const { id = "" } = useParams<{ id: string }>();
  const [misses, setMisses] = useState<TargetCacheMiss[]>([]);
  const [filter, setFilter] = useState("");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    getMisses(id)
      .then((data) => !cancelled && setMisses(data))
      .catch(
        (err) =>
          !cancelled &&
          setError(err instanceof Error ? err.message : "Failed to load"),
      )
      .finally(() => !cancelled && setLoading(false));
    return () => {
      cancelled = true;
    };
  }, [id]);

  const totals = useMemo(() => {
    const missed = misses.reduce((s, m) => s + m.misses, 0);
    const hit = misses.reduce((s, m) => s + m.hits, 0);
    return {
      missed,
      hitRate: missed + hit === 0 ? null : hit / (missed + hit),
      targets: new Set(misses.map((m) => m.label)).size,
    };
  }, [misses]);

  const visible = useMemo(() => {
    const q = filter.trim().toLowerCase();
    return misses.filter(
      (m) =>
        !q ||
        m.label.toLowerCase().includes(q) ||
        m.mnemonic.toLowerCase().includes(q),
    );
  }, [misses, filter]);

  return (
    <div className="space-y-6">
      <PageHeader
        icon={SearchXIcon}
        title="Cache misses"
        description={
          <>
            Actions the ActionCache could not serve in build{" "}
            <span className="font-mono text-xs">{id}</span>
          </>
        }
      >
        <Button variant="outline" size="sm" asChild>
          <Link to={`/builds/${encodeURIComponent(id)}`}>
            <ArrowLeftIcon /> Build
          </Link>
        </Button>
      </PageHeader>

      {loading ? (
        <Skeleton className="h-72 rounded-xl" />
      ) : error ? (
        <p className="text-sm text-destructive">{error}</p>
      ) : misses.length === 0 ? (
        <Card>
          <EmptyState icon={CircleCheckIcon} title="Every action was cached">
            The ActionCache served everything this build asked for, or the build
            recorded no cache lookups.
          </EmptyState>
        </Card>
      ) : (
        <>
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
            <Stat
              icon={CpuIcon}
              label="Executed actions"
              value={totals.missed.toLocaleString()}
            />
            <Stat
              icon={CircleCheckIcon}
              label="Cache hit rate"
              value={formatPercent(totals.hitRate)}
            />
            <Stat
              icon={TargetIcon}
              label="Targets with misses"
              value={totals.targets.toLocaleString()}
            />
          </div>

          <Card className="gap-4">
            <CardHeader>
              <CardTitle>Where the cache misses</CardTitle>
              <CardDescription>
                Executed actions per mnemonic, the top {TOP_MNEMONICS}.
              </CardDescription>
            </CardHeader>
            <CardContent>
              <MnemonicBreakdown misses={misses} />
            </CardContent>
          </Card>

          <Card className="gap-0 pb-0">
            <CardHeader className="pb-4">
              <CardTitle>By target</CardTitle>
              <CardDescription>Most misses first.</CardDescription>
              <CardAction>
                <SearchInput
                  value={filter}
                  onChange={setFilter}
                  placeholder="Label or mnemonic…"
                />
              </CardAction>
            </CardHeader>
            <CardContent className="px-0">
              <div className="max-h-[36rem] overflow-auto border-t">
                <Table>
                  <TableHeader className="sticky top-0 bg-card">
                    <TableRow>
                      <TableHead className="pl-6">Target</TableHead>
                      <TableHead>Mnemonic</TableHead>
                      <TableHead className="text-right">Misses</TableHead>
                      <TableHead className="pr-6 text-right">Hits</TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {visible.map((miss, i) => (
                      <TableRow key={`${miss.label}-${miss.mnemonic}-${i}`}>
                        <TableCell className="max-w-xl pl-6">
                          <Link
                            to={`/targets/${encodeURIComponent(miss.label)}`}
                            className="block truncate font-mono text-xs hover:text-primary"
                            title={miss.label}
                          >
                            {miss.label}
                          </Link>
                        </TableCell>
                        <TableCell>
                          <Badge variant="secondary" className="font-mono">
                            {miss.mnemonic}
                          </Badge>
                        </TableCell>
                        <TableCell className="text-right font-semibold tabular-nums">
                          {miss.misses.toLocaleString()}
                        </TableCell>
                        <TableCell className="pr-6 text-right text-muted-foreground tabular-nums">
                          {miss.hits.toLocaleString()}
                        </TableCell>
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              </div>
            </CardContent>
          </Card>
        </>
      )}
    </div>
  );
}
