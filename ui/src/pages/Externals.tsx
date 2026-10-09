import { Fragment, useEffect, useState } from "react";
import { useSearchParams } from "react-router-dom";
import {
  CheckIcon,
  ChevronRightIcon,
  CopyIcon,
  DownloadIcon,
  ExternalLinkIcon,
  GlobeIcon,
  HardDriveIcon,
  LayersIcon,
  LockIcon,
  PackageSearchIcon,
  RefreshCwIcon,
} from "lucide-react";
import { cn } from "cn";
import { getExternals } from "@/api/bes";
import EcosystemIcon, {
  ecosystemColor,
  ecosystemLabel,
} from "@/components/EcosystemIcon";
import Pager from "@/components/Pager";
import { EmptyState, PageHeader, SearchInput } from "@/components/page";
import { useDebounced } from "@/hooks/useDebounced";
import { formatBytes, timeAgo } from "@/lib/format";
import type {
  ArtifactInfo,
  Ecosystem,
  EcosystemTotal,
  ProxyExternal,
} from "@/types";
import { Badge } from "@/shadcn/ui/badge";
import { Button } from "@/shadcn/ui/button";
import { Card, CardContent } from "@/shadcn/ui/card";
import { Skeleton } from "@/shadcn/ui/skeleton";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/shadcn/ui/table";
import { Tabs, TabsList, TabsTrigger } from "@/shadcn/ui/tabs";

const PAGE_SIZE = 50;
const ECOSYSTEMS: Ecosystem[] = [
  "python",
  "npm",
  "go",
  "rust",
  "java",
  "bazel",
  "github",
  "docker",
  "node",
  "other",
];
const SORTS: Sort[] = ["downloads", "newest", "size", "name"];
const POLL_MS = 15000;

type Sort = "downloads" | "size" | "newest" | "name";

const KIND_LABEL: Record<ArtifactInfo["kind"], string> = {
  package: "package",
  index: "index",
  toolchain: "toolchain",
  release: "release",
  source: "source",
  image: "image",
};

function iso(unix: number) {
  return new Date(unix * 1000).toISOString();
}

function urlHost(url: string) {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}

function fileName(url: string) {
  const path = url.split(/[?#]/)[0];
  return decodeURIComponent(path.slice(path.lastIndexOf("/") + 1)) || url;
}

function CopyValue({
  label,
  value,
  wide = false,
}: {
  label: string;
  value: string;
  wide?: boolean;
}) {
  const [copied, setCopied] = useState(false);
  return (
    <div className={cn("min-w-0", wide && "md:col-span-2")}>
      <dt className="text-xs font-medium text-muted-foreground">{label}</dt>
      <dd className="mt-1 flex items-start gap-1.5">
        <span className="min-w-0 font-mono text-xs break-all">{value}</span>
        <Button
          variant="ghost"
          size="icon"
          className="-mt-1.5 size-7 shrink-0"
          title={`Copy ${label.toLowerCase()}`}
          onClick={() =>
            navigator.clipboard.writeText(value).then(() => {
              setCopied(true);
              setTimeout(() => setCopied(false), 1500);
            })
          }
        >
          {copied ? <CheckIcon /> : <CopyIcon />}
        </Button>
      </dd>
    </div>
  );
}

function EcosystemCard({
  total,
  selected,
  onSelect,
}: {
  total: EcosystemTotal;
  selected: boolean;
  onSelect: () => void;
}) {
  const color = ecosystemColor(total.ecosystem);
  return (
    <button
      type="button"
      onClick={onSelect}
      className={cn(
        "flex flex-col gap-3 rounded-xl border bg-card p-4 text-left shadow-xs transition-colors hover:bg-muted/40",
        selected && "border-primary ring-1 ring-primary",
      )}
    >
      <div className="flex items-center gap-2.5">
        <span
          className="flex size-9 items-center justify-center rounded-lg bg-muted"
          style={color ? { backgroundColor: `${color}1f` } : undefined}
        >
          <EcosystemIcon ecosystem={total.ecosystem} className="size-5" />
        </span>
        <div className="min-w-0">
          <p className="font-medium">{ecosystemLabel(total.ecosystem)}</p>
          <p className="text-xs text-muted-foreground">
            last fetched {timeAgo(iso(total.last_fetched_unix))}
          </p>
        </div>
      </div>
      <div className="grid grid-cols-3 gap-2 text-xs">
        <div>
          <p className="text-muted-foreground">Artifacts</p>
          <p className="font-semibold whitespace-nowrap tabular-nums">
            {total.artifacts.toLocaleString()}
          </p>
        </div>
        <div>
          <p className="text-muted-foreground">Size</p>
          <p className="font-semibold whitespace-nowrap tabular-nums">
            {formatBytes(total.bytes)}
          </p>
        </div>
        <div>
          <p className="text-muted-foreground">Served</p>
          <p className="font-semibold whitespace-nowrap tabular-nums">
            {total.downloads.toLocaleString()}
          </p>
        </div>
      </div>
    </button>
  );
}

function ExternalRow({ external }: { external: ProxyExternal }) {
  const [open, setOpen] = useState(false);
  const { artifact } = external;
  const name = artifact.name ?? fileName(external.url);
  return (
    <Fragment>
      <TableRow
        className="cursor-pointer"
        onClick={() => setOpen((o) => !o)}
        data-state={open ? "selected" : undefined}
      >
        <TableCell className="pl-6">
          <div className="flex items-center gap-3">
            <ChevronRightIcon
              className={cn(
                "size-4 shrink-0 text-muted-foreground transition-transform",
                open && "rotate-90",
              )}
            />
            <EcosystemIcon ecosystem={artifact.ecosystem} className="size-5" />
            <div className="min-w-0">
              <div className="flex min-w-0 items-center gap-2">
                <span className="truncate font-medium" title={name}>
                  {name}
                </span>
                {artifact.version && (
                  <Badge
                    variant="secondary"
                    className="max-w-48 shrink-0 truncate font-mono text-xs"
                    title={artifact.version}
                  >
                    {artifact.version}
                  </Badge>
                )}
              </div>
              <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
                <span>{KIND_LABEL[artifact.kind]}</span>
                <span>·</span>
                <span className="truncate">{urlHost(external.url)}</span>
              </div>
            </div>
          </div>
        </TableCell>
        <TableCell className="text-right tabular-nums">
          {formatBytes(external.size)}
        </TableCell>
        <TableCell className="text-right font-semibold tabular-nums">
          {external.downloads.toLocaleString()}
        </TableCell>
        <TableCell
          className="text-right text-muted-foreground"
          title={new Date(external.fetched_at_unix * 1000).toLocaleString()}
        >
          <div>{timeAgo(iso(external.fetched_at_unix))}</div>
          <div className="text-xs">
            {external.immutable ? "added" : "refreshed"}
          </div>
        </TableCell>
        <TableCell className="pr-6 text-right">
          {artifact.docs_url && (
            <Button
              asChild
              variant="ghost"
              size="sm"
              onClick={(e) => e.stopPropagation()}
            >
              <a href={artifact.docs_url} target="_blank" rel="noreferrer">
                Docs <ExternalLinkIcon />
              </a>
            </Button>
          )}
        </TableCell>
      </TableRow>
      {open && (
        <TableRow className="hover:bg-transparent">
          <TableCell
            colSpan={5}
            className="bg-muted/30 px-6 py-4 whitespace-normal"
          >
            <dl className="grid gap-4 md:grid-cols-2">
              <CopyValue label="URL" value={external.url} wide />
              <CopyValue label="SHA-256" value={external.sha256_hex} />
              <div>
                <dt className="text-xs font-medium text-muted-foreground">
                  Cache policy
                </dt>
                <dd className="mt-1 flex items-center gap-1.5 text-sm">
                  {external.immutable ? (
                    <>
                      <LockIcon className="size-3.5" /> Immutable: fetched once,
                      kept until evicted
                    </>
                  ) : (
                    <>
                      <RefreshCwIcon className="size-3.5" /> Mutable: refetched
                      when its TTL expires
                    </>
                  )}
                </dd>
              </div>
              <div>
                <dt className="text-xs font-medium text-muted-foreground">
                  {external.immutable ? "Added" : "Last refreshed"}
                </dt>
                <dd className="mt-1 text-sm">
                  {new Date(external.fetched_at_unix * 1000).toLocaleString()}
                </dd>
              </div>
              {external.content_type && (
                <div>
                  <dt className="text-xs font-medium text-muted-foreground">
                    Content type
                  </dt>
                  <dd className="mt-1 font-mono text-xs">
                    {external.content_type}
                  </dd>
                </div>
              )}
              {external.etag && (
                <CopyValue label="ETag" value={external.etag} />
              )}
            </dl>
          </TableCell>
        </TableRow>
      )}
    </Fragment>
  );
}

export default function Externals() {
  const [externals, setExternals] = useState<ProxyExternal[]>([]);
  const [ecosystems, setEcosystems] = useState<EcosystemTotal[]>([]);
  const [total, setTotal] = useState(0);
  const [page, setPage] = useState(0);
  const [params, setParams] = useSearchParams();
  const [filter, setFilter] = useState(params.get("q") ?? "");
  const ecosystemParam = params.get("ecosystem") as Ecosystem | null;
  const ecosystem =
    ecosystemParam && ECOSYSTEMS.includes(ecosystemParam)
      ? ecosystemParam
      : null;
  const sortParam = params.get("sort") as Sort | null;
  const sort = sortParam && SORTS.includes(sortParam) ? sortParam : "downloads";
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const q = useDebounced(filter, 300);

  /** Filters live in the URL, so a filtered view can be shared as a link. */
  function setParam(key: string, value: string | null) {
    setParams(
      (current) => {
        const next = new URLSearchParams(current);
        if (value) next.set(key, value);
        else next.delete(key);
        return next;
      },
      { replace: true },
    );
  }
  const setEcosystem = (value: Ecosystem | null) =>
    setParam("ecosystem", value);
  const setSort = (value: Sort) =>
    setParam("sort", value === "downloads" ? null : value);

  useEffect(() => {
    setParams(
      (current) => {
        const next = new URLSearchParams(current);
        if (q) next.set("q", q);
        else next.delete("q");
        return next;
      },
      { replace: true },
    );
  }, [q, setParams]);

  useEffect(() => {
    setPage(0);
  }, [q, ecosystem, sort]);

  useEffect(() => {
    let cancelled = false;
    async function load() {
      try {
        setLoading(true);
        const data = await getExternals({
          limit: PAGE_SIZE,
          offset: page * PAGE_SIZE,
          q: q || undefined,
          ecosystem: ecosystem ?? undefined,
          sort,
        });
        if (cancelled) return;
        setExternals(data.items);
        setEcosystems(data.ecosystems ?? []);
        setTotal(data.total);
        setError(null);
      } catch (e) {
        if (cancelled) return;
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        if (!cancelled) setLoading(false);
      }
    }
    load();
    // Only the current page is re-polled: the proxy cache can hold many
    // thousands of artifacts.
    const timer = setInterval(load, POLL_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [page, q, ecosystem, sort]);

  const all = ecosystems.reduce(
    (sum, e) => ({
      artifacts: sum.artifacts + e.artifacts,
      bytes: sum.bytes + e.bytes,
      downloads: sum.downloads + e.downloads,
    }),
    { artifacts: 0, bytes: 0, downloads: 0 },
  );

  return (
    <div className="space-y-6">
      <PageHeader
        icon={GlobeIcon}
        title="Externals"
        description="Every package, toolchain and image CI downloads, served from the pull-through cache instead of the internet."
      >
        {loading && externals.length > 0 && (
          <RefreshCwIcon className="size-4 animate-spin text-muted-foreground" />
        )}
      </PageHeader>

      <div className="flex flex-wrap items-center gap-x-6 gap-y-2 text-sm text-muted-foreground">
        <span className="inline-flex items-center gap-1.5">
          <LayersIcon className="size-4" />
          <strong className="text-foreground tabular-nums">
            {all.artifacts.toLocaleString()}
          </strong>{" "}
          artifacts
        </span>
        <span className="inline-flex items-center gap-1.5">
          <HardDriveIcon className="size-4" />
          <strong className="text-foreground tabular-nums">
            {formatBytes(all.bytes)}
          </strong>{" "}
          on disk
        </span>
        <span className="inline-flex items-center gap-1.5">
          <DownloadIcon className="size-4" />
          <strong className="text-foreground tabular-nums">
            {all.downloads.toLocaleString()}
          </strong>{" "}
          downloads served from cache
        </span>
        {ecosystem && (
          <Button
            variant="outline"
            size="sm"
            className="ml-auto"
            onClick={() => setEcosystem(null)}
          >
            Show all ecosystems
          </Button>
        )}
      </div>

      {ecosystems.length > 0 ? (
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-5">
          {ecosystems.map((e) => (
            <EcosystemCard
              key={e.ecosystem}
              total={e}
              selected={ecosystem === e.ecosystem}
              onSelect={() =>
                setEcosystem(ecosystem === e.ecosystem ? null : e.ecosystem)
              }
            />
          ))}
        </div>
      ) : (
        loading && (
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-5">
            {Array.from({ length: 5 }, (_, i) => (
              <Skeleton key={i} className="h-28 rounded-xl" />
            ))}
          </div>
        )
      )}

      <Card className="gap-0 py-0">
        <div className="flex flex-wrap items-center justify-between gap-3 border-b px-6 py-4">
          <div className="flex items-center gap-2 text-sm text-muted-foreground">
            {ecosystem && (
              <EcosystemIcon ecosystem={ecosystem} className="size-4" />
            )}
            <span>
              {total.toLocaleString()}{" "}
              {ecosystem ? ecosystemLabel(ecosystem) : ""} artifacts
              {q && <> matching “{q}”</>}
            </span>
          </div>
          <div className="flex flex-wrap items-center gap-3">
            <Tabs value={sort} onValueChange={(v) => setSort(v as Sort)}>
              <TabsList>
                <TabsTrigger value="downloads">Most used</TabsTrigger>
                <TabsTrigger value="newest">Newest</TabsTrigger>
                <TabsTrigger value="size">Heaviest</TabsTrigger>
                <TabsTrigger value="name">Name</TabsTrigger>
              </TabsList>
            </Tabs>
            <SearchInput
              value={filter}
              onChange={setFilter}
              placeholder="Package name or URL…"
              className="sm:w-72"
            />
          </div>
        </div>
        <CardContent className="px-0">
          {error ? (
            <p className="px-6 py-6 text-sm text-destructive">{error}</p>
          ) : loading && externals.length === 0 ? (
            <div className="space-y-2 p-6">
              {Array.from({ length: 6 }, (_, i) => (
                <Skeleton key={i} className="h-12" />
              ))}
            </div>
          ) : externals.length === 0 ? (
            <EmptyState icon={PackageSearchIcon} title="No cached externals">
              {filter || ecosystem
                ? "Nothing cached matches the filters."
                : "Downloads that go through the proxy are listed here."}
            </EmptyState>
          ) : (
            <Table>
              <TableHeader className="bg-muted/40">
                <TableRow>
                  <TableHead className="pl-6">Package</TableHead>
                  <TableHead className="text-right">Size</TableHead>
                  <TableHead className="text-right">Downloads</TableHead>
                  <TableHead className="text-right">Cached</TableHead>
                  <TableHead className="pr-6" />
                </TableRow>
              </TableHeader>
              <TableBody>
                {externals.map((e) => (
                  <ExternalRow key={e.sha256_hex + e.url} external={e} />
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
        <Pager
          page={page}
          pageSize={PAGE_SIZE}
          total={total}
          loading={loading}
          onPage={setPage}
        />
      </Card>
    </div>
  );
}
