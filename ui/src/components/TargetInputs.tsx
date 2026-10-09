import { Link } from "react-router-dom";
import { LightbulbIcon, PackageIcon } from "lucide-react";
import { formatBytes } from "@/lib/format";
import type { ActionInputs } from "@/types";
import { Badge } from "@/shadcn/ui/badge";
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/shadcn/ui/card";

/** What in an action's inputs is worth a developer's look, if anything. */
function hints(inputs: ActionInputs): string[] {
  const names = inputs.groups.map((g) => g.name);
  const out: string[] = [];
  const libcPairs = names.filter(
    (n) => n.endsWith("-musl") && names.includes(n.replace(/-musl$/, "-gnu")),
  );
  if (libcPairs.length > 0)
    out.push(
      `Both libc builds of ${libcPairs.map((n) => n.replace(/^npm:|-musl$/g, "")).join(", ")} are inputs; only one can run. pnpm's supportedArchitectures can install just glibc.`,
    );
  const typeTools = names.filter((n) =>
    ["npm:typescript", "npm:@ts-morph/common", "npm:ts-morph"].includes(n),
  );
  if (inputs.mnemonic === "ViteBuild" && typeTools.length > 0)
    out.push(
      `${typeTools.map((n) => n.replace(/^npm:/, "")).join(" and ")} reach this Vite build. Vite does not type-check; they usually come from a CLI (like shadcn) listed in the build deps.`,
    );
  return out;
}

function groupLabel(name: string) {
  if (name.startsWith("npm:")) return name.slice(4);
  if (name.startsWith("external/rules_nodejs")) return "Node.js toolchain";
  return name;
}

/**
 * The inputs of a target's heaviest remotely executed action: every file
 * here is hashed and placed on a worker each time the action runs, so the
 * deps it does not read are pure cost.
 */
export default function TargetInputs({
  inputs,
  invocationId,
}: {
  inputs: ActionInputs;
  invocationId: string;
}) {
  const advice = hints(inputs);
  return (
    <Card className="gap-4">
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <PackageIcon className="size-4" />
          What its heaviest action drags in
        </CardTitle>
        <CardDescription>
          <span className="font-mono">{inputs.mnemonic}</span> needed{" "}
          <strong className="text-foreground">
            {inputs.files.toLocaleString()} files · {formatBytes(inputs.bytes)}
          </strong>{" "}
          of inputs, hashed and placed on a worker every time it runs. Deps it
          does not read only make it slower and its cache key more fragile.
        </CardDescription>
        <CardAction>
          <Link
            to={`/builds/${encodeURIComponent(invocationId)}`}
            className="font-mono text-xs hover:text-primary"
          >
            {invocationId.slice(0, 13)}…
          </Link>
        </CardAction>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="space-y-2.5">
          {inputs.groups.map((group) => (
            <div key={group.name} className="space-y-1">
              <div className="flex items-baseline justify-between gap-4 text-sm">
                <span className="min-w-0 truncate font-mono text-xs">
                  {groupLabel(group.name)}
                  {group.name.startsWith("external/rules_nodejs") && (
                    <Badge variant="secondary" className="ml-2 font-sans">
                      cached on the worker
                    </Badge>
                  )}
                </span>
                <span className="shrink-0 text-xs text-muted-foreground tabular-nums">
                  {group.files.toLocaleString()}{" "}
                  {group.files === 1 ? "file" : "files"} ·{" "}
                  <span className="font-semibold text-foreground">
                    {formatBytes(group.bytes)}
                  </span>{" "}
                  · {Math.round((group.bytes / inputs.bytes) * 100)}%
                </span>
              </div>
              <div className="h-1.5 overflow-hidden rounded-full bg-muted">
                <div
                  className="h-full rounded-full bg-primary/80"
                  style={{ width: `${(group.bytes / inputs.bytes) * 100}%` }}
                />
              </div>
            </div>
          ))}
        </div>
        {advice.length > 0 && (
          <ul className="space-y-2 rounded-lg border border-warning/40 bg-warning/10 p-3 text-sm">
            {advice.map((text) => (
              <li key={text} className="flex gap-2">
                <LightbulbIcon className="mt-0.5 size-4 shrink-0" />
                {text}
              </li>
            ))}
          </ul>
        )}
      </CardContent>
    </Card>
  );
}
