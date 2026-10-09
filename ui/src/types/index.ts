export type BuildStatus = "in_progress" | "success" | "failure" | "interrupted";

export interface BuildSummary {
  invocation_id: string;
  command: string;
  workspace: string;
  start_time: string;
  end_time?: string;
  duration_ms?: number;
  status: BuildStatus;
  total_actions: number;
  cached_actions: number;
  remote_cache_hits: number;
  local_actions: number;
  action_cache_hits: number;
  failed_actions: number;
  /** Spawns per Bazel runner (`remote`, `linux-sandbox`, `remote cache hit`,
   * ...); absent until the build reports its metrics. */
  runners?: Record<string, number>;
  /** Absent in listings and live updates; the detail endpoint carries it. */
  targets?: string[];
  target_count?: number;
  errors: string[];
  pattern: string[];
  metadata: Record<string, string>;
}

export interface Page<T> {
  items: T[];
  total: number;
}

export interface BuildPage {
  builds: BuildSummary[];
  next_cursor?: string | null;
  total: number;
}

export interface GlobalStats {
  total_builds: number;
  in_progress_builds: number;
  successful_builds: number;
  failed_builds: number;
  interrupted_builds: number;
  total_actions: number;
  failed_actions: number;
}

export interface BuildEvent {
  id?: string | number;
  kind: string;
  timestamp?: string;
  payload: Record<string, unknown>;
}

export interface FileReference {
  name: string;
  uri?: string | null;
  digest: string;
  length: number;
}

export interface FailedAction {
  label: string;
  type: string;
  exit_code?: number | null;
  stdout?: FileReference | null;
  stderr?: FileReference | null;
}

export interface TargetCacheMiss {
  label: string;
  mnemonic: string;
  misses: number;
  hits: number;
}

export type TargetExecutionStatus =
  | "success"
  | "failure"
  | "cached"
  | "dependency";

export interface TargetExecution {
  label: string;
  target_kind: string;
  invocation_id: string;
  status: TargetExecutionStatus;
  tags: string[];
  build_start_time?: string;
  build_end_time?: string;
  build_duration_ms: number;
  action_duration_ms: number;
  action_count: number;
  cached_actions: number;
  failed_actions: number;
  env_vars: Record<string, string>;
  cache_hits: number;
  cache_misses: number;
  missed_mnemonics: Record<string, number>;
  /** Inputs of the target's heaviest remotely executed action. */
  heaviest_inputs?: ActionInputs | null;
}

export interface TargetSummary {
  label: string;
  target_kind: string;
  latest_status: TargetExecutionStatus;
  total_executions: number;
  success_count: number;
  failure_count: number;
  cached_count: number;
  action_count: number;
  cached_actions: number;
  failed_actions: number;
  tags: string[];
}

export interface TestExecution {
  label: string;
  invocation_id: string;
  status: string;
  cached_locally: boolean;
  cached_remotely: boolean;
  start_time?: string;
  end_time?: string;
  duration_ms: number;
}

export interface TestSummary {
  label: string;
  latest_status: string;
  total_runs: number;
  passed_count: number;
  failed_count: number;
  cached_count: number;
  flaky_count: number;
}

export interface WorkerView {
  worker_id: string;
  worker_type: string;
  labels: string[];
  state: string;
  current_execution?: string | null;
  heartbeat_age_secs: number;
}

export interface ProxyExternal {
  url: string;
  size: number;
  sha256_hex: string;
  content_type?: string | null;
  etag?: string | null;
  fetched_at_unix: number;
  immutable: boolean;
  downloads: number;
  artifact: ArtifactInfo;
}

export type Ecosystem =
  | "python"
  | "npm"
  | "go"
  | "rust"
  | "java"
  | "bazel"
  | "github"
  | "docker"
  | "node"
  | "other";

/** What a cached URL is, read from the URL by the server. */
export interface ArtifactInfo {
  ecosystem: Ecosystem;
  kind: "package" | "index" | "toolchain" | "release" | "source" | "image";
  name?: string | null;
  version?: string | null;
  docs_url?: string | null;
}

export interface EcosystemTotal {
  ecosystem: Ecosystem;
  artifacts: number;
  bytes: number;
  downloads: number;
  last_fetched_unix: number;
}

export interface ExternalsPage extends Page<ProxyExternal> {
  /** Totals of every artifact matching the search, whatever the ecosystem. */
  ecosystems: EcosystemTotal[];
}

export type LiveTopic = "builds" | "workers" | { build: { id: string } };

export type LiveEvent =
  | { type: "build_updated"; build: BuildSummary }
  | {
      type: "target_completed";
      invocation_id: string;
      label: string;
      success: boolean;
    }
  | {
      type: "targets_updated";
      invocation_id: string;
      targets: TargetExecution[];
    }
  | {
      type: "log_appended";
      invocation_id: string;
      from: number;
      lines: string[];
    }
  | { type: "action_failed"; invocation_id: string; action: unknown }
  | { type: "workers_updated"; workers: WorkerView[] }
  | { type: "resync" }
  | { type: "pong" }
  | { type: "error"; message: string };

/** What a build was started with. See `GET /builds/{id}/inputs`; secret
 * values arrive redacted. */
export interface BuildInputs {
  command_line: string[];
  startup_options: string[];
  /** Every command option, rc files included, in canonical form. */
  options: string[];
  /** Only the options given on the command line; empty for older builds. */
  explicit_options: string[];
  env: Record<string, string>;
  workspace_status: Record<string, string>;
}

/** A build's Bazel profile summarized for a Gantt chart. See
 * `GET /builds/{id}/profile`; times are ms from the command start. */
/** Where a profiled action ran: `remote` on the RBE workers, `sandbox` or
 * `local` on the machine that ran Bazel, `cache` served from a cache,
 * `internal` without a process; `unknown` in summaries saved before. */
export type Executor =
  | "remote"
  | "sandbox"
  | "local"
  | "cache"
  | "internal"
  | "unknown";

export interface BuildProfile {
  total_ms: number;
  phases: { name: string; start_ms: number }[];
  lanes: number;
  actions: ProfileAction[];
  omitted_actions: number;
  critical_path: { name: string; start_ms: number; duration_ms: number }[];
  /** Actions and time per executor over every action, omitted ones too. */
  executors?: Partial<Record<Executor, { actions: number; total_ms: number }>>;
  /** Actions and time per RBE worker pod, over every remote action matched. */
  workers?: Record<string, { actions: number; total_ms: number }>;
}

export interface ProfileAction {
  name: string;
  mnemonic: string;
  target?: string | null;
  output?: string | null;
  start_ms: number;
  duration_ms: number;
  lane: number;
  critical: boolean;
  executor?: Executor;
  /** The RBE worker pod that ran a remote action. */
  worker?: string | null;
}

/** What a worker materialized before running one action. */
export interface ActionInputs {
  mnemonic: string;
  files: number;
  bytes: number;
  /** Heaviest groups: `npm:<package>`, `external/<repo>` or a directory. */
  groups: { name: string; files: number; bytes: number }[];
}
