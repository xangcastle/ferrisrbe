export type BuildStatus = 'in_progress' | 'success' | 'failure';

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
  failed_actions: number;
  targets: string[];
  errors: string[];
}

export interface GlobalStats {
  total_builds: number;
  in_progress_builds: number;
  successful_builds: number;
  failed_builds: number;
  total_actions: number;
  failed_actions: number;
}

export interface BuildEvent {
  id?: string | number;
  kind: string;
  timestamp?: string;
  payload: Record<string, unknown>;
}

export interface ActionExecutedEvent {
  label: string;
  type: string;
  cached?: boolean;
  success?: boolean;
  remote_cache_hit?: boolean;
  exit_code?: number;
  stdout_digest?: string;
  stderr_digest?: string;
  duration_ms?: number;
}

export type TargetExecutionStatus = 'success' | 'failure' | 'cached';

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
