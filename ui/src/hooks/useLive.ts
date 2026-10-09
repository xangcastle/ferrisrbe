import { useCallback, useEffect, useRef, useState } from "react";
import {
  getBuild,
  getBuilds,
  getBuildTargets,
  getEvents,
  getLog,
  getStats,
} from "../api/bes";
import { live, type LiveStatus } from "../lib/live";
import type {
  BuildEvent,
  BuildSummary,
  FailedAction,
  GlobalStats,
  LiveEvent,
  TargetExecution,
  WorkerView,
} from "../types";

export function useLiveStatus() {
  const [status, setStatus] = useState<LiveStatus>(live.status);
  useEffect(() => live.onStatus(setStatus), []);
  return status;
}

function upsertBuild(list: BuildSummary[], build: BuildSummary) {
  const rest = list.filter((b) => b.invocation_id !== build.invocation_id);
  return [build, ...rest].sort((a, b) =>
    (b.start_time ?? "").localeCompare(a.start_time ?? ""),
  );
}

/**
 * First page of builds plus dashboard stats, kept current from the `builds`
 * topic. `loadMore` appends the next page; the server pages by cursor.
 */
export function useLiveBuilds(pageSize = 50) {
  const [builds, setBuilds] = useState<BuildSummary[]>([]);
  const [total, setTotal] = useState(0);
  const [nextCursor, setNextCursor] = useState<string | null>(null);
  const [stats, setStats] = useState<GlobalStats | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const snapshot = useCallback(async () => {
    try {
      const [page, statsData] = await Promise.all([
        getBuilds({ limit: pageSize }),
        getStats(),
      ]);
      setBuilds(page.builds);
      setTotal(page.total);
      setNextCursor(page.next_cursor ?? null);
      setStats(statsData);
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load builds");
    } finally {
      setLoading(false);
    }
  }, [pageSize]);

  const loadMore = useCallback(async () => {
    if (!nextCursor || loadingMore) return;
    setLoadingMore(true);
    try {
      const page = await getBuilds({ limit: pageSize, cursor: nextCursor });
      setBuilds((list) => {
        const known = new Set(list.map((b) => b.invocation_id));
        return [
          ...list,
          ...page.builds.filter((b) => !known.has(b.invocation_id)),
        ];
      });
      setTotal(page.total);
      setNextCursor(page.next_cursor ?? null);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load builds");
    } finally {
      setLoadingMore(false);
    }
  }, [nextCursor, loadingMore, pageSize]);

  useEffect(() => {
    snapshot();
    return live.subscribe("builds", (event: LiveEvent) => {
      if (event.type === "build_updated") {
        setBuilds((list) => upsertBuild(list, event.build));
        if (event.build.status !== "in_progress") getStats().then(setStats);
      } else if (event.type === "resync") {
        snapshot();
      }
    });
  }, [snapshot]);

  return {
    builds,
    total,
    hasMore: nextCursor !== null,
    loadMore,
    loadingMore,
    stats,
    loading,
    error,
  };
}

function failedActionsOf(events: BuildEvent[]): FailedAction[] {
  return events
    .filter((e) => e.kind === "ActionExecuted")
    .map(
      (e) => e.payload.ActionExecuted as FailedAction & { success?: boolean },
    )
    .filter((a) => a && a.success === false);
}

function spliceLog(current: string, from: number, lines: string[]) {
  const kept = current === "" ? [] : current.split("\n");
  kept.length = Math.min(kept.length, from);
  return [...kept, ...lines].join("\n");
}

/** One build with its targets, log and failures, updated as it runs. */
export function useLiveBuild(id: string | undefined) {
  const [build, setBuild] = useState<BuildSummary | null>(null);
  const [targets, setTargets] = useState<TargetExecution[]>([]);
  const [events, setEvents] = useState<BuildEvent[]>([]);
  const [failures, setFailures] = useState<FailedAction[]>([]);
  const [log, setLog] = useState("");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const pendingLog = useRef<{ from: number; lines: string[] }[]>([]);
  const flush = useRef<number | null>(null);

  const snapshot = useCallback(async () => {
    if (!id) return;
    try {
      const [buildData, eventsData, targetsData, logText] = await Promise.all([
        getBuild(id),
        getEvents(id),
        getBuildTargets(id),
        getLog(id),
      ]);
      setBuild(buildData);
      setEvents(eventsData);
      setFailures(failedActionsOf(eventsData));
      setTargets(targetsData);
      setLog(logText);
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load build");
    } finally {
      setLoading(false);
    }
  }, [id]);

  useEffect(() => {
    if (!id) return;
    setLoading(true);
    snapshot();
    const applyLog = () => {
      flush.current = null;
      const chunks = pendingLog.current;
      pendingLog.current = [];
      setLog((current) =>
        chunks.reduce((acc, c) => spliceLog(acc, c.from, c.lines), current),
      );
    };
    return live.subscribe({ build: { id } }, (event: LiveEvent) => {
      switch (event.type) {
        case "build_updated":
          if (event.build.invocation_id === id) setBuild(event.build);
          break;
        case "target_completed":
          setTargets((list) =>
            list.some((t) => t.label === event.label)
              ? list
              : [
                  ...list,
                  {
                    label: event.label,
                    target_kind: "",
                    invocation_id: id,
                    status: event.success ? "success" : "failure",
                    tags: [],
                    build_duration_ms: 0,
                    action_duration_ms: 0,
                    action_count: 0,
                    cached_actions: 0,
                    failed_actions: 0,
                    env_vars: {},
                    cache_hits: 0,
                    cache_misses: 0,
                    missed_mnemonics: {},
                  },
                ],
          );
          break;
        case "targets_updated":
          setTargets(event.targets);
          break;
        case "log_appended":
          // Bazel emits one Progress per action; coalesce to one render per frame.
          pendingLog.current.push({ from: event.from, lines: event.lines });
          if (flush.current === null)
            flush.current = requestAnimationFrame(applyLog);
          break;
        case "action_failed":
          setFailures((list) => [...list, event.action as FailedAction]);
          setEvents((list) => [
            ...list,
            {
              kind: "ActionExecuted",
              payload: { ActionExecuted: event.action },
            },
          ]);
          break;
        case "resync":
          snapshot();
          break;
      }
    });
  }, [id, snapshot]);

  return { build, targets, events, failures, log, loading, error };
}

/**
 * The last `maxLines` console lines of a build, kept current from its
 * `log_appended` events. Lines are numbered from the start of the console:
 * `base` is how many were dropped from the front.
 */
export function useLiveLogTail(id: string, maxLines = 200) {
  const [tail, setTail] = useState<{ base: number; lines: string[] }>({
    base: 0,
    lines: [],
  });

  useEffect(() => {
    const trim = (base: number, lines: string[]) =>
      lines.length > maxLines
        ? {
            base: base + lines.length - maxLines,
            lines: lines.slice(-maxLines),
          }
        : { base, lines };
    const load = () =>
      getLog(id)
        .then((text) => setTail(trim(0, text === "" ? [] : text.split("\n"))))
        .catch(() => {});
    load();
    return live.subscribe({ build: { id } }, (event: LiveEvent) => {
      if (event.type === "log_appended" && event.invocation_id === id) {
        setTail(({ base, lines }) =>
          event.from >= base
            ? trim(base, [...lines.slice(0, event.from - base), ...event.lines])
            : trim(event.from, event.lines),
        );
      } else if (event.type === "resync") {
        load();
      }
    });
  }, [id, maxLines]);

  return tail.lines.join("\n");
}

export function useLiveWorkers() {
  const [workers, setWorkers] = useState<WorkerView[] | null>(null);
  useEffect(
    () =>
      live.subscribe("workers", (event: LiveEvent) => {
        if (event.type === "workers_updated") setWorkers(event.workers);
      }),
    [],
  );
  return workers;
}
