import type { BuildSummary } from "@/types";

/** "850ms", "42s", "4m 12s", "1h 05m". */
export function formatDuration(ms?: number | null): string {
  if (ms === undefined || ms === null) return "–";
  if (ms < 1000) return `${ms}ms`;
  const totalSecs = Math.floor(ms / 1000);
  const hours = Math.floor(totalSecs / 3600);
  const minutes = Math.floor((totalSecs % 3600) / 60);
  const seconds = totalSecs % 60;
  if (hours > 0) return `${hours}h ${String(minutes).padStart(2, "0")}m`;
  if (minutes > 0) return `${minutes}m ${String(seconds).padStart(2, "0")}s`;
  return `${seconds}s`;
}

/** Time from `startIso` to `nowMs`, as [`formatDuration`] shows it. */
export function formatElapsed(startIso: string | undefined, nowMs: number) {
  const started = startIso ? new Date(startIso).getTime() : NaN;
  if (Number.isNaN(started)) return "–";
  return formatDuration(Math.max(0, nowMs - started));
}

/** "just now", "5m ago", "3h ago", "2d ago". */
export function timeAgo(iso?: string | null, nowMs = Date.now()): string {
  const at = iso ? new Date(iso).getTime() : NaN;
  if (Number.isNaN(at)) return "–";
  const secs = Math.max(0, Math.floor((nowMs - at) / 1000));
  if (secs < 45) return "just now";
  if (secs < 3600) return `${Math.round(secs / 60)}m ago`;
  if (secs < 86400) return `${Math.round(secs / 3600)}h ago`;
  return `${Math.round(secs / 86400)}d ago`;
}

/** Share of spawned actions served from a cache, or null with none spawned. */
export function cacheHitRatio(build: BuildSummary): number | null {
  const spawns = build.cached_actions + build.local_actions;
  return spawns === 0 ? null : build.cached_actions / spawns;
}

export function formatPercent(ratio: number | null): string {
  return ratio === null ? "–" : `${Math.round(ratio * 100)}%`;
}

/** "512 B", "3.2 MiB", "1.4 GiB". */
export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let value = bytes / 1024;
  let unit = units[0];
  for (const next of units.slice(1)) {
    if (value < 1024) break;
    value /= 1024;
    unit = next;
  }
  return `${value.toFixed(1)} ${unit}`;
}
