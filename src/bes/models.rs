//! Serializable models for the BES REST API, the live channel and the
//! on-disk build records.
//!
//! Field names are part of the JSON contract with the UI. Timestamps are
//! RFC 3339 strings and durations are milliseconds throughout.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// High-level status of a build invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildStatus {
    InProgress,
    Success,
    Failure,
    /// The stream ended without `BuildFinished`, as when the server
    /// restarted while the build ran.
    Interrupted,
}

/// Summary of a single build invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildSummary {
    pub invocation_id: String,
    /// The Bazel command, e.g. `build` or `test`.
    pub command: String,
    pub workspace: String,
    pub start_time: Option<String>,
    /// Absent while the build is in progress.
    pub end_time: Option<String>,
    pub duration_ms: u64,
    pub status: BuildStatus,
    pub total_actions: u64,
    pub cached_actions: u64,
    pub remote_cache_hits: u64,
    pub local_actions: u64,
    /// Actions served from the ActionCache of this server.
    #[serde(default)]
    pub action_cache_hits: u64,
    pub failed_actions: u64,
    /// Bazel's spawn count per runner, as its "N processes: ..." line names
    /// them: `remote` ran on the RBE workers; `linux-sandbox`, `local`,
    /// `worker` (persistent workers) and the other sandboxes ran on the
    /// machine that ran Bazel; `* cache hit` were served from a cache and
    /// `internal` spawned no process. Empty until `BuildMetrics` arrives.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub runners: BTreeMap<String, u64>,
    /// Labels of the targets the build configured.
    pub targets: Vec<String>,
    /// Error messages reported by Bazel.
    pub errors: Vec<String>,
    /// Target patterns the user asked for, e.g. `//...`.
    #[serde(default)]
    pub pattern: Vec<String>,
    /// `--build_metadata` key/values (USER, BRANCH_NAME, PR_NUMBER, ...).
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl Default for BuildSummary {
    fn default() -> Self {
        Self {
            invocation_id: String::new(),
            command: String::new(),
            workspace: String::new(),
            start_time: None,
            end_time: None,
            duration_ms: 0,
            status: BuildStatus::InProgress,
            total_actions: 0,
            cached_actions: 0,
            remote_cache_hits: 0,
            local_actions: 0,
            action_cache_hits: 0,
            failed_actions: 0,
            runners: BTreeMap::new(),
            targets: Vec::new(),
            errors: Vec::new(),
            pattern: Vec::new(),
            metadata: BTreeMap::new(),
        }
    }
}

/// A build as the listing and the live channel carry it: a [`BuildSummary`]
/// with its target list reduced to `target_count`, since the list can run to
/// thousands of labels per build.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildListItem {
    pub invocation_id: String,
    pub command: String,
    pub workspace: String,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub duration_ms: u64,
    pub status: BuildStatus,
    pub total_actions: u64,
    pub cached_actions: u64,
    pub remote_cache_hits: u64,
    pub local_actions: u64,
    #[serde(default)]
    pub action_cache_hits: u64,
    pub failed_actions: u64,
    #[serde(default)]
    pub target_count: usize,
    pub errors: Vec<String>,
    #[serde(default)]
    pub pattern: Vec<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl From<&BuildSummary> for BuildListItem {
    fn from(summary: &BuildSummary) -> Self {
        Self {
            invocation_id: summary.invocation_id.clone(),
            command: summary.command.clone(),
            workspace: summary.workspace.clone(),
            start_time: summary.start_time.clone(),
            end_time: summary.end_time.clone(),
            duration_ms: summary.duration_ms,
            status: summary.status,
            total_actions: summary.total_actions,
            cached_actions: summary.cached_actions,
            remote_cache_hits: summary.remote_cache_hits,
            local_actions: summary.local_actions,
            action_cache_hits: summary.action_cache_hits,
            failed_actions: summary.failed_actions,
            target_count: summary.targets.len(),
            errors: summary.errors.clone(),
            pattern: summary.pattern.clone(),
            metadata: summary.metadata.clone(),
        }
    }
}

impl Default for BuildListItem {
    fn default() -> Self {
        Self::from(&BuildSummary::default())
    }
}

/// One page of the build listing, newest first. `next_cursor` is opaque and
/// absent on the last page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildPage {
    pub builds: Vec<BuildListItem>,
    pub next_cursor: Option<String>,
    pub total: usize,
}

/// One page of a server-sorted listing; `total` counts every match, not only
/// the items on this page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: usize,
}

/// What a build was started with: its command line as typed, the options
/// Bazel parsed from it and the rc files, the client environment and the
/// workspace status. Secret values are already redacted.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BuildInputs {
    /// `UnstructuredCommandLine`: every argument, startup options first.
    pub command_line: Vec<String>,
    pub startup_options: Vec<String>,
    /// Every command option, rc files included, in canonical form.
    pub options: Vec<String>,
    /// The options given explicitly on the command line; empty for builds
    /// stored before they were recorded.
    pub explicit_options: Vec<String>,
    /// `--client_env` of the command line.
    pub env: BTreeMap<String, String>,
    /// `WorkspaceStatus` items: BUILD_USER, BUILD_HOST, git state...
    pub workspace_status: BTreeMap<String, String>,
}

/// Global statistics across all recorded invocations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalStats {
    pub total_builds: usize,
    pub in_progress_builds: usize,
    pub successful_builds: usize,
    pub failed_builds: usize,
    #[serde(default)]
    pub interrupted_builds: usize,
    pub total_actions: u64,
    pub failed_actions: u64,
}

/// Body of `GET /api/health`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
}

impl Default for HealthResponse {
    fn default() -> Self {
        Self {
            status: "ok".to_string(),
        }
    }
}

/// High-level status of a single target execution within a build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetExecutionStatus {
    Success,
    Failure,
    Cached,
    /// Not requested on the command line; seen only through its actions.
    Dependency,
}

/// Aggregated view of a target across multiple builds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetSummary {
    pub label: String,
    /// The rule kind, e.g. `rust_library rule`.
    pub target_kind: String,
    /// Status in the most recent build that ran the target.
    pub latest_status: TargetExecutionStatus,
    pub total_executions: usize,
    pub success_count: usize,
    pub failure_count: usize,
    pub cached_count: usize,
    #[serde(default)]
    pub action_count: u64,
    #[serde(default)]
    pub cached_actions: u64,
    #[serde(default)]
    pub failed_actions: u64,
    /// Summed [`TargetExecution::action_duration_ms`] over its executions.
    #[serde(default)]
    pub total_action_ms: u64,
    /// The longest [`TargetExecution::action_duration_ms`].
    #[serde(default)]
    pub max_action_ms: u64,
    /// The mean over the executions that ran actions, cached ones aside.
    #[serde(default)]
    pub mean_action_ms: u64,
    pub tags: Vec<String>,
}

/// A single execution record for a target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetExecution {
    pub label: String,
    pub target_kind: String,
    pub invocation_id: String,
    pub status: TargetExecutionStatus,
    pub tags: Vec<String>,
    pub build_start_time: Option<String>,
    pub build_end_time: Option<String>,
    /// Wall time of the whole build the execution belongs to.
    #[serde(default)]
    pub build_duration_ms: u64,
    /// Summed duration of the target's own actions; zero when Bazel publishes
    /// no action events for it.
    #[serde(default)]
    pub action_duration_ms: u64,
    #[serde(default)]
    pub action_count: u64,
    #[serde(default)]
    pub cached_actions: u64,
    #[serde(default)]
    pub failed_actions: u64,
    /// Environment variables of the invocation, taken from its command
    /// line.
    #[serde(default)]
    pub env_vars: std::collections::HashMap<String, String>,
    /// ActionCache lookups for this target's actions that were served.
    #[serde(default)]
    pub cache_hits: u64,
    /// ActionCache lookups for this target's actions that missed.
    #[serde(default)]
    pub cache_misses: u64,
    /// Misses per action mnemonic.
    #[serde(default)]
    pub missed_mnemonics: BTreeMap<String, u64>,
    /// The inputs of this target's heaviest remotely executed action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heaviest_inputs: Option<crate::cache::action_cache_log::ActionInputs>,
}

/// One kind of action of a target that the ActionCache could not serve.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetCacheMiss {
    pub label: String,
    pub mnemonic: String,
    pub misses: u64,
    pub hits: u64,
}

/// Action metrics accumulated for one target while its build streams in.
#[derive(Debug, Clone, Default)]
pub struct TargetActionMetrics {
    pub action_count: u64,
    pub cached_actions: u64,
    pub failed_actions: u64,
    pub duration_ms: u64,
}

/// The actions of one mnemonic within a single build, aggregated for the
/// build timeline.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MnemonicTimeline {
    pub mnemonic: String,
    pub action_count: u64,
    pub failed_count: u64,
    /// Sum of each action's own duration. Exceeds the build's wall time when
    /// actions of the mnemonic ran in parallel.
    pub total_duration_ms: u64,
    /// Earliest start observed for the mnemonic.
    pub min_start: Option<String>,
    /// Latest end observed for the mnemonic.
    pub max_end: Option<String>,
    /// Target label of the mnemonic's slowest action.
    pub longest_label: Option<String>,
    /// Duration of the mnemonic's slowest action.
    pub longest_duration_ms: u64,
}

/// A single test execution record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestExecution {
    pub label: String,
    pub invocation_id: String,
    /// Bazel's `TestStatus` name, e.g. `PASSED` or `FLAKY`.
    pub status: String,
    pub cached_locally: bool,
    pub cached_remotely: bool,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub duration_ms: u64,
}

/// Aggregated view of a test target across multiple builds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestSummary {
    pub label: String,
    pub latest_status: String,
    pub total_runs: usize,
    pub passed_count: usize,
    pub failed_count: usize,
    pub cached_count: usize,
    pub flaky_count: usize,
    /// Runs Bazel reported without a status: interrupted builds, and records
    /// stored before the status was read from `TestResult`.
    #[serde(default)]
    pub no_status_count: usize,
}
