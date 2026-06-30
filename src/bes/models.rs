//! Serializable models for the BES REST API.

use serde::{Deserialize, Serialize};

/// High-level status of a build invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildStatus {
    InProgress,
    Success,
    Failure,
}

/// Summary of a single build invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildSummary {
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
    pub failed_actions: u64,
    pub targets: Vec<String>,
    pub errors: Vec<String>,
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
            failed_actions: 0,
            targets: Vec::new(),
            errors: Vec::new(),
        }
    }
}

/// Global statistics across all recorded invocations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalStats {
    pub total_builds: usize,
    pub in_progress_builds: usize,
    pub successful_builds: usize,
    pub failed_builds: usize,
    pub total_actions: u64,
    pub failed_actions: u64,
}

/// Health check response.
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
}

/// Aggregated view of a target across multiple builds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetSummary {
    pub label: String,
    pub target_kind: String,
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
    #[serde(default)]
    pub build_duration_ms: u64,
    /// Duration of the target's own actions, when action events are published.
    #[serde(default)]
    pub action_duration_ms: u64,
    #[serde(default)]
    pub action_count: u64,
    #[serde(default)]
    pub cached_actions: u64,
    #[serde(default)]
    pub failed_actions: u64,
    #[serde(default)]
    pub env_vars: std::collections::HashMap<String, String>,
}

/// Aggregated action metrics for a target execution.
#[derive(Debug, Clone, Default)]
pub struct TargetActionMetrics {
    pub action_count: u64,
    pub cached_actions: u64,
    pub failed_actions: u64,
    pub duration_ms: u64,
}

/// A single test execution record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestExecution {
    pub label: String,
    pub invocation_id: String,
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
}
