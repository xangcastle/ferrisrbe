import { useEffect, useMemo, useState } from "react";
import { CheckIcon, CopyIcon, LockIcon } from "lucide-react";
import { getInputs } from "@/api/bes";
import { SearchInput } from "@/components/page";
import type { BuildInputs, BuildSummary } from "@/types";
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
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/shadcn/ui/tabs";

const REDACTED = "<redacted>";

/** Arguments the Bazel client adds for the server, not typed by anyone. */
const CLIENT_PLUMBING = [
  "--client_env=",
  "--default_override=",
  "--rc_source=",
  "--ignore_all_rc_files",
  "--isatty=",
  "--terminal_columns=",
  "--client_cwd=",
  "--client_debug=",
];

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <Button
      variant="outline"
      size="sm"
      onClick={() => {
        navigator.clipboard.writeText(text).then(() => {
          setCopied(true);
          setTimeout(() => setCopied(false), 1500);
        });
      }}
    >
      {copied ? <CheckIcon /> : <CopyIcon />}
      {copied ? "Copied" : "Copy"}
    </Button>
  );
}

/** `--name=value` → [name, value]; `--name` / `--noname` → [name, ""]. */
function splitOption(option: string): [string, string] {
  const eq = option.indexOf("=");
  return eq < 0 ? [option, ""] : [option.slice(0, eq), option.slice(eq + 1)];
}

/** The command as typed: bazel, the command, explicit options, patterns. */
function typedCommand(build: BuildSummary, inputs: BuildInputs) {
  const options =
    inputs.explicit_options.length > 0
      ? inputs.explicit_options
      : inputs.command_line
          .slice(inputs.command_line.indexOf(build.command) + 1)
          .filter(
            (arg) =>
              arg.startsWith("-") &&
              !CLIENT_PLUMBING.some((prefix) => arg.startsWith(prefix)),
          );
  return ["bazel", build.command, ...options, ...(build.pattern ?? [])].join(
    " ",
  );
}

function CommandPanel({
  build,
  inputs,
}: {
  build: BuildSummary;
  inputs: BuildInputs;
}) {
  const command = typedCommand(build, inputs);
  const [showRaw, setShowRaw] = useState(false);
  return (
    <div className="space-y-4">
      <Card className="gap-4">
        <CardHeader>
          <CardTitle>Command</CardTitle>
          <CardDescription>
            As typed, without what the rc files add.
          </CardDescription>
          <CardAction>
            <CopyButton text={command} />
          </CardAction>
        </CardHeader>
        <CardContent>
          <pre className="overflow-x-auto rounded-lg bg-neutral-950 p-4 font-mono text-sm leading-6 whitespace-pre-wrap text-neutral-100">
            <span className="select-none text-neutral-500">$ </span>
            {command}
          </pre>
        </CardContent>
      </Card>
      <Card className="gap-4">
        <CardHeader>
          <CardTitle>Raw command line</CardTitle>
          <CardDescription>
            The {inputs.command_line.length} arguments the Bazel client sent the
            server, rc expansions and client environment included.
          </CardDescription>
          <CardAction>
            <Button
              variant="ghost"
              size="sm"
              onClick={() => setShowRaw((v) => !v)}
            >
              {showRaw ? "Hide" : "Show"}
            </Button>
          </CardAction>
        </CardHeader>
        {showRaw && (
          <CardContent>
            <ol className="max-h-[28rem] overflow-auto rounded-lg border bg-muted/30 py-2 font-mono text-xs leading-6">
              {inputs.command_line.map((arg, i) => (
                <li key={i} className="flex gap-3 px-3 hover:bg-muted">
                  <span className="w-8 shrink-0 text-right text-muted-foreground select-none">
                    {i}
                  </span>
                  <span className="break-all">{arg}</span>
                </li>
              ))}
            </ol>
          </CardContent>
        )}
      </Card>
    </div>
  );
}

function OptionsTable({
  title,
  description,
  options,
  explicit,
}: {
  title: string;
  description: string;
  options: string[];
  explicit?: Set<string>;
}) {
  const [query, setQuery] = useState("");
  const rows = useMemo(() => {
    const q = query.trim().toLowerCase();
    return options
      .map((option) => ({ option, parts: splitOption(option) }))
      .filter(({ option }) => !q || option.toLowerCase().includes(q))
      .sort(
        (a, b) =>
          Number(explicit?.has(b.option) ?? 0) -
          Number(explicit?.has(a.option) ?? 0),
      );
  }, [options, query, explicit]);
  return (
    <Card className="gap-0 pb-0">
      <CardHeader className="pb-4">
        <CardTitle>{title}</CardTitle>
        <CardDescription>{description}</CardDescription>
        <CardAction>
          <SearchInput
            value={query}
            onChange={setQuery}
            placeholder="Filter options"
          />
        </CardAction>
      </CardHeader>
      <CardContent className="px-0">
        {rows.length === 0 ? (
          <p className="px-6 pb-6 text-sm text-muted-foreground">No options.</p>
        ) : (
          <div className="max-h-[32rem] overflow-auto border-t">
            <Table>
              <TableHeader className="sticky top-0 bg-card">
                <TableRow>
                  <TableHead className="pl-6">Option</TableHead>
                  <TableHead>Value</TableHead>
                  {explicit && (
                    <TableHead className="pr-6 text-right">Source</TableHead>
                  )}
                </TableRow>
              </TableHeader>
              <TableBody>
                {rows.map(({ option, parts: [name, value] }, i) => (
                  <TableRow key={`${option}-${i}`}>
                    <TableCell className="pl-6 font-mono text-xs">
                      {name}
                    </TableCell>
                    <TableCell className="max-w-xl font-mono text-xs break-all whitespace-normal">
                      {value || (
                        <span className="text-muted-foreground">–</span>
                      )}
                    </TableCell>
                    {explicit && (
                      <TableCell className="pr-6 text-right">
                        {explicit.has(option) ? (
                          <Badge>command line</Badge>
                        ) : (
                          <Badge variant="secondary">rc / config</Badge>
                        )}
                      </TableCell>
                    )}
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

function EnvironmentPanel({ env }: { env: Record<string, string> }) {
  const [query, setQuery] = useState("");
  const entries = useMemo(() => {
    const q = query.trim().toLowerCase();
    return Object.entries(env)
      .sort(([a], [b]) => a.localeCompare(b))
      .filter(
        ([k, v]) =>
          !q || k.toLowerCase().includes(q) || v.toLowerCase().includes(q),
      );
  }, [env, query]);
  const redacted = Object.values(env).filter((v) => v === REDACTED).length;
  return (
    <Card className="gap-0 pb-0">
      <CardHeader className="pb-4">
        <CardTitle>Environment</CardTitle>
        <CardDescription>
          The client environment Bazel forwarded ({Object.keys(env).length}{" "}
          variables
          {redacted > 0 && `, ${redacted} redacted`}). Names holding TOKEN,
          SECRET, PASSWORD, KEY... never leave the server with their value.
        </CardDescription>
        <CardAction>
          <SearchInput
            value={query}
            onChange={setQuery}
            placeholder="Filter variables"
          />
        </CardAction>
      </CardHeader>
      <CardContent className="px-0">
        <div className="max-h-[32rem] overflow-auto border-t">
          <Table>
            <TableHeader className="sticky top-0 bg-card">
              <TableRow>
                <TableHead className="pl-6">Variable</TableHead>
                <TableHead className="pr-6">Value</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {entries.map(([name, value]) => (
                <TableRow key={name}>
                  <TableCell className="pl-6 font-mono text-xs">
                    {name}
                  </TableCell>
                  <TableCell className="max-w-2xl pr-6 font-mono text-xs break-all whitespace-normal">
                    {value === REDACTED ? (
                      <Badge variant="outline" className="gap-1">
                        <LockIcon className="size-3" /> redacted
                      </Badge>
                    ) : (
                      value
                    )}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
      </CardContent>
    </Card>
  );
}

export function useBuildInputs(id: string) {
  const [inputs, setInputs] = useState<BuildInputs | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let cancelled = false;
    setInputs(null);
    getInputs(id)
      .then((data) => !cancelled && setInputs(data))
      .catch(
        (e) =>
          !cancelled && setError(e instanceof Error ? e.message : String(e)),
      );
    return () => {
      cancelled = true;
    };
  }, [id]);
  return { inputs, error };
}

export default function InputsTab({
  build,
  inputs,
  error,
}: {
  build: BuildSummary;
  inputs: BuildInputs | null;
  error: string | null;
}) {
  if (error) return <p className="text-sm text-destructive">Inputs: {error}</p>;
  if (!inputs) return <Skeleton className="h-64 rounded-xl" />;
  const explicit = new Set(inputs.explicit_options);
  return (
    <Tabs defaultValue="command" className="gap-4">
      <TabsList>
        <TabsTrigger value="command">Command</TabsTrigger>
        <TabsTrigger value="flags">
          Flags
          <span className="text-muted-foreground tabular-nums">
            {inputs.options.length}
          </span>
        </TabsTrigger>
        <TabsTrigger value="env">
          Environment
          <span className="text-muted-foreground tabular-nums">
            {Object.keys(inputs.env).length}
          </span>
        </TabsTrigger>
      </TabsList>
      <TabsContent value="command">
        <CommandPanel build={build} inputs={inputs} />
      </TabsContent>
      <TabsContent value="flags" className="space-y-4">
        <OptionsTable
          title="Command options"
          description={
            explicit.size > 0
              ? "Every option in effect, from the command line or expanded from the rc files and --config."
              : "Every option in effect. This build predates the record of which ones were typed."
          }
          options={inputs.options}
          explicit={explicit.size > 0 ? explicit : undefined}
        />
        <OptionsTable
          title="Startup options"
          description="Options of the Bazel server itself, before the command."
          options={inputs.startup_options}
        />
      </TabsContent>
      <TabsContent value="env">
        <EnvironmentPanel env={inputs.env} />
      </TabsContent>
    </Tabs>
  );
}
