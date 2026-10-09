import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { cn } from "cn";
import { getProfile } from "@/api/bes";
import { formatDuration } from "@/lib/format";
import type { BuildProfile, Executor, ProfileAction } from "@/types";

/** The build's profile; `profile` is null once loaded when there is none. */
export function useBuildProfile(id: string | undefined) {
  const [state, setState] = useState<{
    profile: BuildProfile | null;
    loading: boolean;
    error: string | null;
  }>({ profile: null, loading: true, error: null });
  useEffect(() => {
    if (!id) return;
    let cancelled = false;
    setState({ profile: null, loading: true, error: null });
    getProfile(id)
      .then(
        (profile) =>
          !cancelled && setState({ profile, loading: false, error: null }),
      )
      .catch(
        (e) =>
          !cancelled &&
          setState({
            profile: null,
            loading: false,
            error: e instanceof Error ? e.message : String(e),
          }),
      );
    return () => {
      cancelled = true;
    };
  }, [id]);
  return state;
}

/** Cool hues only, so nothing reads as the orange of the critical path. */
const PALETTE = [
  "bg-teal-500",
  "bg-sky-500",
  "bg-violet-500",
  "bg-emerald-600",
  "bg-pink-500",
  "bg-indigo-400",
];

/** The mnemonics that took the most time, each with its color. */
export function mnemonicColors(actions: ProfileAction[]) {
  const totals = new Map<string, number>();
  for (const a of actions)
    totals.set(a.mnemonic, (totals.get(a.mnemonic) ?? 0) + a.duration_ms);
  const ranked = [...totals.entries()].sort((a, b) => b[1] - a[1]);
  const colors = new Map<string, string>();
  ranked
    .slice(0, PALETTE.length)
    .forEach(([m], i) => colors.set(m, PALETTE[i]));
  return {
    ranked,
    color: (m: string) => colors.get(m) ?? "bg-muted-foreground/40",
  };
}

/** How each executor is named and drawn, in legend order. */
export const EXECUTORS: { executor: Executor; label: string; color: string }[] =
  [
    { executor: "remote", label: "RBE worker", color: "bg-sky-500" },
    { executor: "sandbox", label: "Host · sandbox", color: "bg-teal-500" },
    { executor: "local", label: "Host · local", color: "bg-lime-600" },
    { executor: "cache", label: "Cache", color: "bg-muted-foreground/50" },
    {
      executor: "internal",
      label: "Internal",
      color: "bg-muted-foreground/25",
    },
    { executor: "unknown", label: "Unknown", color: "bg-muted-foreground/40" },
  ];

export function executorLabel(executor: Executor | undefined) {
  return EXECUTORS.find((e) => e.executor === (executor ?? "unknown"))!.label;
}

function executorColor(executor: Executor | undefined) {
  return EXECUTORS.find((e) => e.executor === (executor ?? "unknown"))!.color;
}

export type ColorBy = "mnemonic" | "executor" | "worker";

/** The workers that ran the most action time, each with its color. */
export function workerColors(profile: BuildProfile) {
  const ranked = Object.entries(profile.workers ?? {}).sort(
    (a, b) => b[1].total_ms - a[1].total_ms,
  );
  const colors = new Map<string, string>();
  ranked
    .slice(0, PALETTE.length)
    .forEach(([w], i) => colors.set(w, PALETTE[i]));
  return {
    ranked,
    color: (w: string | null | undefined) =>
      (w && colors.get(w)) ?? "bg-muted-foreground/40",
  };
}

function ticks(totalMs: number) {
  const step = totalMs / 5;
  return Array.from({ length: 6 }, (_, i) => i * step);
}

/**
 * Actions of a Bazel profile on their lanes over time. Critical-path
 * actions are drawn in the primary color; the rest by mnemonic, or by where
 * they ran. Clicking an action opens its target.
 */
export default function ProfileGantt({
  profile,
  actions,
  from = 0,
  to = profile.total_ms,
  rowHeight = 12,
  maxHeight = "28rem",
  colorBy = "mnemonic",
}: {
  profile: BuildProfile;
  actions: ProfileAction[];
  from?: number;
  to?: number;
  rowHeight?: number;
  maxHeight?: string;
  colorBy?: ColorBy;
}) {
  const navigate = useNavigate();
  const span = Math.max(to - from, 1);
  const { color } = useMemo(() => mnemonicColors(actions), [actions]);
  const workers = useMemo(() => workerColors(profile), [profile]);
  const lanes = useMemo(() => {
    const first = new Map<number, number>();
    for (const a of actions)
      first.set(a.lane, Math.min(first.get(a.lane) ?? Infinity, a.start_ms));
    return [...first.entries()]
      .sort((a, b) => a[1] - b[1])
      .map(([lane]) => lane);
  }, [actions]);
  const row = useMemo(
    () => new Map(lanes.map((lane, i) => [lane, i])),
    [lanes],
  );
  const x = (ms: number) => `${((ms - from) / span) * 100}%`;

  return (
    <div className="space-y-1">
      <div className="relative ml-1 h-5 text-[10px] text-muted-foreground">
        {ticks(span).map((t) => (
          <span
            key={t}
            className="absolute -translate-x-1/2 tabular-nums first:translate-x-0 last:-translate-x-full"
            style={{ left: `${(t / span) * 100}%` }}
          >
            {formatDuration(Math.round(t))}
          </span>
        ))}
      </div>
      <div
        className="relative overflow-y-auto rounded-lg border bg-muted/30"
        style={{ maxHeight }}
      >
        <div
          className="relative"
          style={{ height: Math.max(lanes.length, 1) * rowHeight + 8 }}
        >
          {profile.phases
            .filter((p) => p.start_ms > from && p.start_ms < to)
            .map((p) => (
              <div
                key={p.name}
                title={p.name}
                className="absolute top-0 bottom-0 border-l border-dashed border-foreground/20"
                style={{ left: x(p.start_ms) }}
              />
            ))}
          {actions.map((a, i) => (
            <button
              key={i}
              type="button"
              title={`${a.name}\n${a.mnemonic}${a.target ? ` · ${a.target}` : ""}\n${formatDuration(Math.round(a.duration_ms))} · ${executorLabel(a.executor)}${a.worker ? ` on ${a.worker}` : ""}${a.critical ? " · critical path" : ""}`}
              onClick={() =>
                a.target && navigate(`/targets/${encodeURIComponent(a.target)}`)
              }
              className={cn(
                "absolute rounded-[2px] transition-opacity hover:opacity-100",
                a.critical
                  ? "z-10 bg-primary opacity-100 ring-1 ring-primary"
                  : cn(
                      colorBy === "executor"
                        ? executorColor(a.executor)
                        : colorBy === "worker"
                          ? workers.color(a.worker)
                          : color(a.mnemonic),
                      "opacity-75",
                    ),
              )}
              style={{
                left: x(Math.max(a.start_ms, from)),
                width: `max(2px, ${(Math.min(a.duration_ms, to - a.start_ms) / span) * 100}%)`,
                top: 4 + (row.get(a.lane) ?? 0) * rowHeight,
                height: rowHeight - 3,
              }}
            />
          ))}
        </div>
      </div>
    </div>
  );
}

/** Steps of the critical path with their share of the build. */
export function CriticalPathList({ profile }: { profile: BuildProfile }) {
  const total = profile.critical_path.reduce((s, c) => s + c.duration_ms, 0);
  if (profile.critical_path.length === 0)
    return (
      <p className="text-sm text-muted-foreground">
        Bazel reported no critical path for this build.
      </p>
    );
  return (
    <ol className="space-y-2">
      {profile.critical_path.map((step, i) => {
        const action = profile.actions.find(
          (a) => a.critical && `action '${a.name}'` === step.name,
        );
        return (
          <li key={i} className="space-y-1">
            <div className="flex items-baseline justify-between gap-4 text-sm">
              <span className="min-w-0 truncate">
                <span className="mr-2 text-muted-foreground tabular-nums">
                  {i + 1}.
                </span>
                {step.name.replace(/^action '(.*)'$/, "$1")}
                {action?.executor && action.executor !== "unknown" && (
                  <span className="ml-2 inline-flex items-center gap-1 text-xs text-muted-foreground">
                    <span
                      className={cn(
                        "size-1.5 rounded-full",
                        executorColor(action.executor),
                      )}
                    />
                    {executorLabel(action.executor)}
                    {action.worker && (
                      <span className="font-mono">· {action.worker}</span>
                    )}
                  </span>
                )}
                {action?.target && (
                  <span className="ml-2 font-mono text-xs text-muted-foreground">
                    {action.target}
                  </span>
                )}
              </span>
              <span className="shrink-0 tabular-nums">
                {formatDuration(Math.round(step.duration_ms))}
                <span className="ml-2 text-xs text-muted-foreground">
                  {total > 0
                    ? `${Math.round((step.duration_ms / total) * 100)}%`
                    : ""}
                </span>
              </span>
            </div>
            <div className="h-1.5 overflow-hidden rounded-full bg-muted">
              <div
                className="h-full rounded-full bg-primary"
                style={{
                  width: `${total > 0 ? (step.duration_ms / total) * 100 : 0}%`,
                }}
              />
            </div>
          </li>
        );
      })}
    </ol>
  );
}

/** Total action time per mnemonic, with the colors the Gantt uses. */
export function MnemonicLegend({ actions }: { actions: ProfileAction[] }) {
  const { ranked, color } = useMemo(() => mnemonicColors(actions), [actions]);
  return (
    <div className="flex flex-wrap gap-x-4 gap-y-1.5 text-xs">
      <span className="inline-flex items-center gap-1.5">
        <span className="size-2 rounded-full bg-primary" />
        critical path
      </span>
      {ranked.slice(0, 8).map(([mnemonic, ms]) => (
        <span key={mnemonic} className="inline-flex items-center gap-1.5">
          <span className={cn("size-2 rounded-full", color(mnemonic))} />
          <span className="font-mono">{mnemonic}</span>
          <span className="text-muted-foreground tabular-nums">
            {formatDuration(Math.round(ms))}
          </span>
        </span>
      ))}
    </div>
  );
}

/** Actions and time per executor, with the colors the Gantt uses. */
export function ExecutorLegend({ profile }: { profile: BuildProfile }) {
  const totals = profile.executors ?? {};
  return (
    <div className="flex flex-wrap gap-x-4 gap-y-1.5 text-xs">
      <span className="inline-flex items-center gap-1.5">
        <span className="size-2 rounded-full bg-primary" />
        critical path
      </span>
      {EXECUTORS.filter((e) => totals[e.executor]).map((e) => {
        const total = totals[e.executor]!;
        return (
          <span key={e.executor} className="inline-flex items-center gap-1.5">
            <span className={cn("size-2 rounded-full", e.color)} />
            {e.label}
            <span className="text-muted-foreground tabular-nums">
              {total.actions.toLocaleString()} ·{" "}
              {formatDuration(Math.round(total.total_ms))}
            </span>
          </span>
        );
      })}
    </div>
  );
}

/** Actions and time per RBE worker pod, with the colors the Gantt uses. */
export function WorkerLegend({ profile }: { profile: BuildProfile }) {
  const { ranked, color } = useMemo(() => workerColors(profile), [profile]);
  return (
    <div className="flex flex-wrap gap-x-4 gap-y-1.5 text-xs">
      <span className="inline-flex items-center gap-1.5">
        <span className="size-2 rounded-full bg-primary" />
        critical path
      </span>
      {ranked.map(([worker, total]) => (
        <span key={worker} className="inline-flex items-center gap-1.5">
          <span className={cn("size-2 rounded-full", color(worker))} />
          <span className="font-mono">{worker}</span>
          <span className="text-muted-foreground tabular-nums">
            {total.actions.toLocaleString()} ·{" "}
            {formatDuration(Math.round(total.total_ms))}
          </span>
        </span>
      ))}
      <span className="text-muted-foreground">
        Grey: not run by an RBE worker.
      </span>
    </div>
  );
}
