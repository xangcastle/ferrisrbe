//! BES event storage and summary derivation.

// Several BEP fields used here are marked deprecated by Bazel but still
// emitted by current build tools, so we intentionally read them.
#![allow(deprecated)]

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use prost::Message;
use serde_json::Value;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};

use crate::bes::config::BesConfig;
use crate::bes::models::{
    BuildStatus, BuildSummary, GlobalStats, TargetActionMetrics, TargetExecution,
    TargetExecutionStatus, TargetSummary as TargetSummaryModel, TestExecution,
    TestSummary as TestSummaryModel,
};
use crate::proto::build::bazel::build_event_stream::build_event::Payload;
use crate::proto::build::bazel::build_event_stream::{
    build_event_id, ActionExecuted, BuildEvent, BuildEventId, BuildFinished, BuildMetrics,
    BuildStarted, TargetComplete, TestResult, TestSummary as ProtoTestSummary,
};
use crate::proto::tools::protos::SpawnExec;

/// Errors that can occur in BES storage.
#[derive(Debug, thiserror::Error)]
pub enum BesError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Invocation not found: {0}")]
    InvocationNotFound(String),
}

pub type Result<T> = std::result::Result<T, BesError>;

/// In-memory state for an active invocation.
#[derive(Debug, Clone)]
struct InvocationState {
    summary: BuildSummary,
    events: Vec<Value>,
    action_metrics: HashMap<String, TargetActionMetrics>,
    finalized: bool,
    indexed: bool,
    target_kinds: HashMap<String, String>,
}

/// Persistent storage for BES invocations.
#[derive(Debug, Clone)]
pub struct BesStorage {
    config: BesConfig,
    invocations: Arc<DashMap<String, InvocationState>>,
    targets: Arc<DashMap<String, Vec<TargetExecution>>>,
    tests: Arc<DashMap<String, Vec<TestExecution>>>,
}

impl BesStorage {
    /// Create a new storage backend, ensuring the data directory exists.
    pub async fn new(config: BesConfig) -> Result<Self> {
        fs::create_dir_all(&config.data_dir).await?;
        let storage = Self {
            config,
            invocations: Arc::new(DashMap::new()),
            targets: Arc::new(DashMap::new()),
            tests: Arc::new(DashMap::new()),
        };
        storage.load_targets_from_disk().await?;
        storage.load_tests_from_disk().await?;
        Ok(storage)
    }

    /// Store a build event for an invocation and update the derived summary.
    pub async fn store_event(&self, invocation_id: &str, event: BuildEvent) -> Result<()> {
        let event_json = build_event_to_json(&event);
        let is_final = event.last_message;

        // Ensure invocation directory exists.
        fs::create_dir_all(self.config.invocation_dir(invocation_id)).await?;

        // Append event to JSONL.
        let events_path = self.config.events_path(invocation_id);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .write(true)
            .open(&events_path)
            .await?;
        let line = serde_json::to_string(&event_json)?;
        file.write_all(line.as_bytes()).await?;
        file.write_all(b"\n").await?;
        file.flush().await?;
        drop(file);

        // Update in-memory summary.
        let mut entry = self
            .invocations
            .entry(invocation_id.to_string())
            .or_insert_with(|| InvocationState {
                summary: BuildSummary {
                    invocation_id: invocation_id.to_string(),
                    ..Default::default()
                },
                events: Vec::new(),
                action_metrics: HashMap::new(),
                finalized: false,
                indexed: false,
                target_kinds: HashMap::new(),
            });
        entry.events.push(event_json.clone());
        update_summary(&mut entry.summary, &event);

        // Track target kinds as they arrive; index targets/tests once the build
        // is complete so we can correlate actions, env vars, and timings.
        match &event.payload {
            Some(Payload::Configured(configured)) => {
                if let Some(id) = &event.id {
                    if let Some(label) = target_label_from_id(id) {
                        entry
                            .target_kinds
                            .insert(label, configured.target_kind.clone());
                    }
                }
            }
            Some(Payload::Action(action)) => {
                update_action_metrics(&mut entry.action_metrics, action);
            }
            _ => {}
        }

        let is_finished = matches!(&event.payload, Some(Payload::Finished(_)));
        if (is_final || is_finished) && !entry.indexed {
            entry.indexed = true;
            entry.finalized = true;
            let summary_path = self.config.summary_path(invocation_id);
            let summary_json = serde_json::to_string_pretty(&entry.summary)?;
            fs::write(&summary_path, summary_json).await?;
            self.index_invocation_targets(
                invocation_id,
                &entry.events,
                &entry.target_kinds,
                &entry.summary,
                &entry.action_metrics,
            )
            .await?;
            debug!("Finalized invocation {}", invocation_id);
        }

        Ok(())
    }

    /// Mark an invocation as finalized, writing its summary to disk.
    pub async fn finalize(&self, invocation_id: &str) -> Result<()> {
        if let Some(mut entry) = self.invocations.get_mut(invocation_id) {
            entry.finalized = true;
            let summary_path = self.config.summary_path(invocation_id);
            let summary_json = serde_json::to_string_pretty(&entry.summary)?;
            fs::write(&summary_path, summary_json).await?;
        }
        Ok(())
    }

    /// List all recorded builds, newest first.
    pub async fn list_builds(&self) -> Result<Vec<BuildSummary>> {
        let mut summaries = Vec::new();
        let mut entries = fs::read_dir(&self.config.data_dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let invocation_id = match path.file_name().and_then(|n| n.to_str()) {
                Some(id) => id.to_string(),
                None => continue,
            };

            if let Ok(Some(summary)) = self.get_build(&invocation_id).await {
                summaries.push(summary);
            } else if let Ok(summary) = self.load_summary_from_disk(&invocation_id).await {
                summaries.push(summary);
            }
        }

        // Sort by start_time descending (newest first); fallback to invocation id.
        summaries.sort_by(|a, b| {
            b.start_time
                .as_ref()
                .cmp(&a.start_time.as_ref())
                .then_with(|| b.invocation_id.cmp(&a.invocation_id))
        });

        Ok(summaries)
    }

    /// Get the summary for a single invocation.
    pub async fn get_build(&self, invocation_id: &str) -> Result<Option<BuildSummary>> {
        if let Some(entry) = self.invocations.get(invocation_id) {
            return Ok(Some(entry.summary.clone()));
        }
        self.load_summary_from_disk(invocation_id).await.map(Some)
    }

    /// Get all stored events for an invocation as JSON values.
    pub async fn get_events(&self, invocation_id: &str) -> Result<Vec<Value>> {
        if let Some(entry) = self.invocations.get(invocation_id) {
            return Ok(entry.events.clone());
        }
        self.load_events_from_disk(invocation_id).await
    }

    /// Get ActionExecuted events that are considered cache misses.
    ///
    /// A cache miss is an action that actually ran: its start and end times
    /// differ, or it failed. Actions with identical start/end times are
    /// treated as cache hits (local or remote).
    pub async fn get_misses(&self, invocation_id: &str) -> Result<Vec<Value>> {
        let events = self.get_events(invocation_id).await?;
        Ok(events
            .into_iter()
            .filter_map(|event| {
                event
                    .get("payload")
                    .and_then(|p| p.get("ActionExecuted").cloned())
                    .map(|action| (action, event.get("id").cloned()))
            })
            .filter(|(action, _)| {
                let success = action
                    .get("success")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let start = action.get("start_time").and_then(|v| v.as_str());
                let end = action.get("end_time").and_then(|v| v.as_str());
                let is_cached = success && start.is_some() && end.is_some() && start == end;
                !is_cached
            })
            .map(|(action, id)| {
                let start = action
                    .get("start_time")
                    .and_then(|v| v.as_str())
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok());
                let end = action
                    .get("end_time")
                    .and_then(|v| v.as_str())
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok());
                let duration_ms = match (start, end) {
                    (Some(s), Some(e)) => Some((e - s).num_milliseconds() as u64),
                    _ => None,
                };
                serde_json::json!({
                    "label": action.get("label").and_then(|v| v.as_str()).unwrap_or(""),
                    "type": action.get("type").and_then(|v| v.as_str()).unwrap_or(""),
                    "success": action.get("success").and_then(|v| v.as_bool()).unwrap_or(false),
                    "remote_cache_hit": false,
                    "cached": false,
                    "exit_code": action.get("exit_code").and_then(|v| v.as_i64()).map(|v| v as i32),
                    "duration_ms": duration_ms,
                    "event_id": id,
                })
            })
            .collect())
    }

    /// Append a target execution to the in-memory index and persist it.
    async fn append_target_execution(&self, execution: TargetExecution) -> Result<()> {
        let targets_path = self.config.targets_path();
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .write(true)
            .open(&targets_path)
            .await?;
        let line = serde_json::to_string(&execution)?;
        file.write_all(line.as_bytes()).await?;
        file.write_all(b"\n").await?;
        file.flush().await?;
        drop(file);

        self.targets
            .entry(execution.label.clone())
            .or_insert_with(Vec::new)
            .push(execution);
        Ok(())
    }

    /// Append a test execution to the in-memory index and persist it.
    async fn append_test_execution(&self, execution: TestExecution) -> Result<()> {
        let tests_path = self.config.tests_path();
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .write(true)
            .open(&tests_path)
            .await?;
        let line = serde_json::to_string(&execution)?;
        file.write_all(line.as_bytes()).await?;
        file.write_all(b"\n").await?;
        file.flush().await?;
        drop(file);

        self.tests
            .entry(execution.label.clone())
            .or_insert_with(Vec::new)
            .push(execution);
        Ok(())
    }

    /// Load target execution history from disk.
    async fn load_targets_from_disk(&self) -> Result<()> {
        let targets_path = self.config.targets_path();
        if !targets_path.exists() {
            return Ok(());
        }
        let content = fs::read_to_string(&targets_path).await?;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<TargetExecution>(line) {
                Ok(execution) => {
                    self.targets
                        .entry(execution.label.clone())
                        .or_insert_with(Vec::new)
                        .push(execution);
                }
                Err(e) => warn!("Failed to parse target execution line: {}", e),
            }
        }
        Ok(())
    }

    /// Load test execution history from disk.
    async fn load_tests_from_disk(&self) -> Result<()> {
        let tests_path = self.config.tests_path();
        if !tests_path.exists() {
            return Ok(());
        }
        let content = fs::read_to_string(&tests_path).await?;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<TestExecution>(line) {
                Ok(execution) => {
                    self.tests
                        .entry(execution.label.clone())
                        .or_insert_with(Vec::new)
                        .push(execution);
                }
                Err(e) => warn!("Failed to parse test execution line: {}", e),
            }
        }
        Ok(())
    }

    /// List all known targets with aggregated summaries.
    pub async fn list_targets(&self) -> Result<Vec<TargetSummaryModel>> {
        let mut summaries: Vec<TargetSummaryModel> = self
            .targets
            .iter()
            .map(|entry| {
                let label = entry.key().clone();
                let executions = entry.value();
                let mut success = 0;
                let mut failure = 0;
                let mut cached = 0;
                let mut action_count = 0;
                let mut cached_actions = 0;
                let mut failed_actions = 0;
                let mut tags = Vec::new();
                let mut kind = String::new();
                let latest_status = executions
                    .last()
                    .map(|e| e.status)
                    .unwrap_or(TargetExecutionStatus::Success);
                for ex in executions.iter() {
                    match ex.status {
                        TargetExecutionStatus::Success => success += 1,
                        TargetExecutionStatus::Failure => failure += 1,
                        TargetExecutionStatus::Cached => cached += 1,
                    }
                    action_count += ex.action_count;
                    cached_actions += ex.cached_actions;
                    failed_actions += ex.failed_actions;
                    if kind.is_empty() && !ex.target_kind.is_empty() {
                        kind = ex.target_kind.clone();
                    }
                    for tag in &ex.tags {
                        if !tags.contains(tag) {
                            tags.push(tag.clone());
                        }
                    }
                }
                TargetSummaryModel {
                    label,
                    target_kind: kind,
                    latest_status,
                    total_executions: executions.len(),
                    success_count: success,
                    failure_count: failure,
                    cached_count: cached,
                    action_count,
                    cached_actions,
                    failed_actions,
                    tags,
                }
            })
            .collect();
        summaries.sort_by(|a, b| {
            b.total_executions
                .cmp(&a.total_executions)
                .then_with(|| a.label.cmp(&b.label))
        });
        Ok(summaries)
    }

    /// Get all target executions for a specific build invocation.
    pub async fn get_build_targets(&self, invocation_id: &str) -> Result<Vec<TargetExecution>> {
        let mut targets: Vec<TargetExecution> = Vec::new();
        for entry in self.targets.iter() {
            for execution in entry.value().iter() {
                if execution.invocation_id == invocation_id {
                    targets.push(execution.clone());
                }
            }
        }
        targets.sort_by(|a, b| a.label.cmp(&b.label));
        Ok(targets)
    }

    /// Get execution history for a single target.
    pub async fn get_target_history(&self, label: &str) -> Result<Vec<TargetExecution>> {
        Ok(self
            .targets
            .get(label)
            .map(|entry| entry.value().clone())
            .unwrap_or_default())
    }

    /// List all known tests with aggregated summaries.
    pub async fn list_tests(&self) -> Result<Vec<TestSummaryModel>> {
        let mut summaries: Vec<TestSummaryModel> = self
            .tests
            .iter()
            .map(|entry| {
                let label = entry.key().clone();
                let executions = entry.value();
                let mut passed = 0;
                let mut failed = 0;
                let mut cached = 0;
                let mut flaky = 0;
                let latest_status = executions
                    .last()
                    .map(|e| e.status.clone())
                    .unwrap_or_else(|| "NO_STATUS".to_string());
                for ex in executions.iter() {
                    match ex.status.as_str() {
                        "PASSED" => passed += 1,
                        "FLAKY" => flaky += 1,
                        "FAILED"
                        | "TIMEOUT"
                        | "INCOMPLETE"
                        | "REMOTE_FAILURE"
                        | "FAILED_TO_BUILD"
                        | "TOOL_HALTED_BEFORE_TESTING" => failed += 1,
                        _ => {}
                    }
                    if ex.cached_locally || ex.cached_remotely {
                        cached += 1;
                    }
                }
                TestSummaryModel {
                    label,
                    latest_status,
                    total_runs: executions.len(),
                    passed_count: passed,
                    failed_count: failed,
                    cached_count: cached,
                    flaky_count: flaky,
                }
            })
            .collect();
        summaries.sort_by(|a, b| {
            b.total_runs
                .cmp(&a.total_runs)
                .then_with(|| a.label.cmp(&b.label))
        });
        Ok(summaries)
    }

    /// Get execution history for a single test.
    pub async fn get_test_history(&self, label: &str) -> Result<Vec<TestExecution>> {
        Ok(self
            .tests
            .get(label)
            .map(|entry| entry.value().clone())
            .unwrap_or_default())
    }

    /// Compute global statistics across all invocations.
    pub async fn stats(&self) -> Result<GlobalStats> {
        let builds = self.list_builds().await?;
        let mut stats = GlobalStats {
            total_builds: builds.len(),
            in_progress_builds: 0,
            successful_builds: 0,
            failed_builds: 0,
            total_actions: 0,
            failed_actions: 0,
        };

        for summary in builds {
            match summary.status {
                BuildStatus::InProgress => stats.in_progress_builds += 1,
                BuildStatus::Success => stats.successful_builds += 1,
                BuildStatus::Failure => stats.failed_builds += 1,
            }
            stats.total_actions += summary.total_actions;
            stats.failed_actions += summary.failed_actions;
        }

        Ok(stats)
    }

    /// Index all target and test executions for an invocation after it finishes.
    async fn index_invocation_targets(
        &self,
        invocation_id: &str,
        events: &[Value],
        target_kinds: &HashMap<String, String>,
        summary: &BuildSummary,
        action_metrics: &HashMap<String, TargetActionMetrics>,
    ) -> Result<()> {
        let env_vars = extract_env_vars(events);

        for event_json in events {
            let payload = event_json.get("payload");
            let id = event_json.get("id").and_then(|v| v.as_str());

            if let (Some(payload), Some(id_str)) = (payload, id) {
                if let Some(completed) = payload.get("Completed") {
                    if let Some(label) = target_label_from_string_id(id_str) {
                        let target_kind = target_kinds.get(&label).cloned().unwrap_or_default();
                        let metrics = action_metrics.get(&label).cloned().unwrap_or_default();
                        let execution = build_target_execution(
                            &label,
                            &target_kind,
                            invocation_id,
                            completed,
                            summary,
                            &metrics,
                            &env_vars,
                        );
                        self.append_target_execution(execution).await?;
                    }
                }

                if let Some(result) = payload.get("TestResult") {
                    if let Some(label) = target_label_from_string_id(id_str) {
                        let execution = build_test_execution_json(&label, invocation_id, result);
                        self.append_test_execution(execution).await?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Load a summary from disk, returning a default if missing.
    async fn load_summary_from_disk(&self, invocation_id: &str) -> Result<BuildSummary> {
        let summary_path = self.config.summary_path(invocation_id);
        if summary_path.exists() {
            let content = fs::read_to_string(&summary_path).await?;
            return Ok(serde_json::from_str(&content)?);
        }
        Ok(BuildSummary {
            invocation_id: invocation_id.to_string(),
            ..Default::default()
        })
    }

    /// Load events from the JSONL file on disk.
    async fn load_events_from_disk(&self, invocation_id: &str) -> Result<Vec<Value>> {
        let events_path = self.config.events_path(invocation_id);
        if !events_path.exists() {
            return Ok(Vec::new());
        }
        let content = fs::read_to_string(&events_path).await?;
        let mut events = Vec::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(line) {
                Ok(value) => events.push(value),
                Err(e) => warn!("Failed to parse event line for {}: {}", invocation_id, e),
            }
        }
        Ok(events)
    }
}

/// Update a build summary from a single BEP event.
fn update_summary(summary: &mut BuildSummary, event: &BuildEvent) {
    let event_id = event.id.as_ref();
    if let Some(id) = event_id {
        if let Some(build_event_id::Id::Started(_)) = &id.id {
            summary.status = BuildStatus::InProgress;
        }
    }

    if let Some(payload) = &event.payload {
        match payload {
            Payload::Started(started) => handle_started(summary, started),
            Payload::Finished(finished) => handle_finished(summary, finished),
            Payload::Action(action) => handle_action(summary, event_id, action),
            Payload::Completed(completed) => handle_completed(summary, event_id, completed),
            Payload::TestResult(result) => handle_test_result(summary, result),
            Payload::TestSummary(test_summary) => handle_test_summary(summary, test_summary),
            Payload::BuildMetrics(metrics) => handle_build_metrics(summary, metrics),
            _ => {}
        }
    }
}

fn handle_started(summary: &mut BuildSummary, started: &BuildStarted) {
    summary.command = started.command.clone();
    summary.workspace = started.workspace_directory.clone();
    if let Some(ts) = &started.start_time {
        summary.start_time = Some(prost_timestamp_to_rfc3339(ts));
    }
}

fn handle_finished(summary: &mut BuildSummary, finished: &BuildFinished) {
    let success = finished.overall_success
        || finished
            .exit_code
            .as_ref()
            .map(|ec| ec.code == 0)
            .unwrap_or(false);
    summary.status = if success {
        BuildStatus::Success
    } else {
        BuildStatus::Failure
    };
    if let Some(ts) = &finished.finish_time {
        summary.end_time = Some(prost_timestamp_to_rfc3339(ts));
    }
    if let (Some(start), Some(end)) = (&summary.start_time, &summary.end_time) {
        if let Ok(start_dt) = DateTime::parse_from_rfc3339(start) {
            if let Ok(end_dt) = DateTime::parse_from_rfc3339(end) {
                summary.duration_ms = (end_dt - start_dt).num_milliseconds() as u64;
            }
        }
    }
    if !success {
        if let Some(detail) = &finished.failure_detail {
            summary.errors.push(format!("{:?}", detail));
        }
    }
}

fn handle_action(
    summary: &mut BuildSummary,
    event_id: Option<&BuildEventId>,
    action: &ActionExecuted,
) {
    summary.total_actions += 1;
    if let Some(id) = event_id {
        if let Some(build_event_id::Id::ActionCompleted(a)) = &id.id {
            if !a.label.is_empty() && !summary.targets.contains(&a.label) {
                summary.targets.push(a.label.clone());
            }
        }
    }
    if action.success {
        // Heuristic: if the action succeeded with a very short or zero-duration
        // execution window, treat it as a cache hit. This is intentionally
        // conservative and will be refined with richer metadata.
        let is_cached = match (&action.start_time, &action.end_time) {
            (Some(start), Some(end)) => start.seconds == end.seconds && start.nanos == end.nanos,
            _ => false,
        };
        if is_cached {
            summary.cached_actions += 1;
        } else {
            summary.local_actions += 1;
        }
    } else {
        summary.failed_actions += 1;
    }
}

fn handle_completed(
    summary: &mut BuildSummary,
    event_id: Option<&BuildEventId>,
    completed: &TargetComplete,
) {
    if let Some(id) = event_id {
        if let Some(build_event_id::Id::TargetCompleted(t)) = &id.id {
            if !t.label.is_empty() && !summary.targets.contains(&t.label) {
                summary.targets.push(t.label.clone());
            }
        }
    }
    if !completed.success {
        if let Some(detail) = &completed.failure_detail {
            summary.errors.push(format!("{:?}", detail));
        }
    }
}

fn handle_test_result(summary: &mut BuildSummary, result: &TestResult) {
    if result.cached_locally {
        summary.cached_actions += 1;
    }
    if let Some(info) = &result.execution_info {
        if info.cached_remotely {
            summary.remote_cache_hits += 1;
        }
    }
}

fn handle_test_summary(summary: &mut BuildSummary, test_summary: &ProtoTestSummary) {
    summary.cached_actions += test_summary.total_num_cached as u64;
}

fn handle_build_metrics(summary: &mut BuildSummary, metrics: &BuildMetrics) {
    if let Some(action_summary) = &metrics.action_summary {
        summary.remote_cache_hits = action_summary.remote_cache_hits as u64;
    }
}

fn prost_timestamp_to_rfc3339(ts: &prost_types::Timestamp) -> String {
    let dt = DateTime::from_timestamp(ts.seconds, ts.nanos as u32).unwrap_or_else(|| Utc::now());
    dt.to_rfc3339()
}

/// Convert a `BuildEvent` into a `serde_json::Value` for JSONL storage and API
/// responses.
///
/// This is intentionally lightweight and serializes the most relevant payloads
/// explicitly; unknown payloads are represented by their variant name.
fn build_event_to_json(event: &BuildEvent) -> Value {
    let mut obj = serde_json::json!({
        "last_message": event.last_message,
    });

    if let Some(id) = &event.id {
        obj["id"] = Value::String(build_event_id_kind(id));
        obj["children"] = Value::Array(event.children.iter().map(build_event_id_to_json).collect());
    }

    if let Some(payload) = &event.payload {
        let kind = payload_kind(payload);
        obj["kind"] = Value::String(kind.clone());
        obj["timestamp"] = payload_timestamp(payload)
            .map(Value::String)
            .unwrap_or(Value::Null);
        obj["payload"] = match payload {
            Payload::Progress(p) => serde_json::json!({
                "Progress": {
                    "stdout": p.stdout,
                    "stderr": p.stderr,
                }
            }),
            Payload::Aborted(a) => serde_json::json!({
                "Aborted": {
                    "reason": a.reason,
                    "description": a.description,
                }
            }),
            Payload::Started(s) => serde_json::json!({
                "Started": {
                    "uuid": s.uuid,
                    "command": s.command,
                    "workspace_directory": s.workspace_directory,
                    "working_directory": s.working_directory,
                    "build_tool_version": s.build_tool_version,
                    "start_time": s.start_time.as_ref().map(prost_timestamp_to_rfc3339),
                }
            }),
            Payload::Expanded(_) => serde_json::json!({"Expanded": {}}),
            Payload::Action(a) => serde_json::json!({
                "ActionExecuted": {
                    "success": a.success,
                    "type": a.r#type,
                    "exit_code": a.exit_code,
                    "label": a.label,
                    "start_time": a.start_time.as_ref().map(prost_timestamp_to_rfc3339),
                    "end_time": a.end_time.as_ref().map(prost_timestamp_to_rfc3339),
                }
            }),
            Payload::Completed(c) => serde_json::json!({
                "Completed": {
                    "success": c.success,
                    "tag": c.tag,
                }
            }),
            Payload::TestSummary(ts) => serde_json::json!({
                "TestSummary": {
                    "overall_status": ts.overall_status,
                    "total_run_count": ts.total_run_count,
                    "total_num_cached": ts.total_num_cached,
                }
            }),
            Payload::TestResult(tr) => serde_json::json!({
                "TestResult": {
                    "status": tr.status,
                    "cached_locally": tr.cached_locally,
                    "cached_remotely": tr.execution_info.as_ref().map(|i| i.cached_remotely),
                }
            }),
            Payload::Finished(f) => serde_json::json!({
                "Finished": {
                    "overall_success": f.overall_success,
                    "exit_code": f.exit_code.as_ref().map(|ec| ec.code),
                    "finish_time": f.finish_time.as_ref().map(prost_timestamp_to_rfc3339),
                }
            }),
            Payload::BuildMetrics(m) => serde_json::json!({
                "BuildMetrics": {
                    "remote_cache_hits": m.action_summary.as_ref().map(|a| a.remote_cache_hits),
                    "actions_executed": m.action_summary.as_ref().map(|a| a.actions_executed),
                }
            }),
            Payload::WorkspaceInfo(w) => serde_json::json!({
                "WorkspaceInfo": {
                    "local_exec_root": w.local_exec_root,
                }
            }),
            Payload::BuildMetadata(m) => serde_json::json!({
                "BuildMetadata": {
                    "metadata": m.metadata,
                }
            }),
            Payload::Configuration(c) => serde_json::json!({
                "Configuration": {
                    "mnemonic": c.mnemonic,
                    "platform_name": c.platform_name,
                    "cpu": c.cpu,
                }
            }),
            Payload::Configured(c) => serde_json::json!({
                "Configured": {
                    "target_kind": c.target_kind,
                    "actual": c.actual,
                }
            }),
            Payload::NamedSetOfFiles(_) => serde_json::json!({"NamedSetOfFiles": {}}),
            Payload::UnstructuredCommandLine(u) => serde_json::json!({
                "UnstructuredCommandLine": {
                    "args": u.args,
                }
            }),
            Payload::OptionsParsed(o) => serde_json::json!({
                "OptionsParsed": {
                    "cmd_line": o.cmd_line,
                    "startup_options": o.startup_options,
                }
            }),
            Payload::WorkspaceStatus(w) => serde_json::json!({
                "WorkspaceStatus": {
                    "item": w.item.iter().map(|i| serde_json::json!({
                        "key": i.key,
                        "value": i.value,
                    })).collect::<Vec<_>>(),
                }
            }),
            Payload::StructuredCommandLine(any) => serde_json::json!({
                "StructuredCommandLine": {
                    "type_url": any.type_url,
                }
            }),
            Payload::BuildToolLogs(_) => serde_json::json!({"BuildToolLogs": {}}),
            Payload::TargetSummary(_) => serde_json::json!({"TargetSummary": {}}),
            Payload::ConvenienceSymlinksIdentified(_) => {
                serde_json::json!({"ConvenienceSymlinksIdentified": {}})
            }
            Payload::ExecRequest(_) => serde_json::json!({"ExecRequest": {}}),
            Payload::TestProgress(_) => serde_json::json!({"TestProgress": {}}),
            Payload::SkyvalueUploaded(_) => serde_json::json!({"SkyvalueUploaded": {}}),
        };
    }

    obj
}

fn payload_kind(payload: &Payload) -> String {
    match payload {
        Payload::Progress(_) => "Progress".to_string(),
        Payload::Aborted(_) => "Aborted".to_string(),
        Payload::Started(_) => "BuildStarted".to_string(),
        Payload::Expanded(_) => "PatternExpanded".to_string(),
        Payload::Action(_) => "ActionExecuted".to_string(),
        Payload::Completed(_) => "TargetComplete".to_string(),
        Payload::TestSummary(_) => "TestSummary".to_string(),
        Payload::TestResult(_) => "TestResult".to_string(),
        Payload::UnstructuredCommandLine(_) => "UnstructuredCommandLine".to_string(),
        Payload::OptionsParsed(_) => "OptionsParsed".to_string(),
        Payload::Finished(_) => "BuildFinished".to_string(),
        Payload::NamedSetOfFiles(_) => "NamedSetOfFiles".to_string(),
        Payload::WorkspaceStatus(_) => "WorkspaceStatus".to_string(),
        Payload::Configuration(_) => "Configuration".to_string(),
        Payload::Configured(_) => "TargetConfigured".to_string(),
        Payload::BuildToolLogs(_) => "BuildToolLogs".to_string(),
        Payload::BuildMetrics(_) => "BuildMetrics".to_string(),
        Payload::WorkspaceInfo(_) => "WorkspaceInfo".to_string(),
        Payload::BuildMetadata(_) => "BuildMetadata".to_string(),
        Payload::TargetSummary(_) => "TargetSummary".to_string(),
        Payload::ConvenienceSymlinksIdentified(_) => "ConvenienceSymlinksIdentified".to_string(),
        Payload::ExecRequest(_) => "ExecRequest".to_string(),
        Payload::TestProgress(_) => "TestProgress".to_string(),
        Payload::StructuredCommandLine(_) => "StructuredCommandLine".to_string(),
        Payload::SkyvalueUploaded(_) => "SkyValueUploaded".to_string(),
    }
}

fn payload_timestamp(payload: &Payload) -> Option<String> {
    match payload {
        Payload::Started(s) => s.start_time.as_ref().map(prost_timestamp_to_rfc3339),
        Payload::Action(a) => a.start_time.as_ref().map(prost_timestamp_to_rfc3339),
        Payload::Finished(f) => f.finish_time.as_ref().map(prost_timestamp_to_rfc3339),
        Payload::TestResult(t) => t
            .test_attempt_start
            .as_ref()
            .map(prost_timestamp_to_rfc3339),
        Payload::TestSummary(t) => t.first_start_time.as_ref().map(prost_timestamp_to_rfc3339),
        _ => None,
    }
}

fn build_event_id_kind(id: &BuildEventId) -> String {
    if let Some(inner) = &id.id {
        match inner {
            build_event_id::Id::Unknown(_) => "Unknown".to_string(),
            build_event_id::Id::Progress(_) => "Progress".to_string(),
            build_event_id::Id::Started(_) => "BuildStarted".to_string(),
            build_event_id::Id::BuildFinished(_) => "BuildFinished".to_string(),
            build_event_id::Id::ActionCompleted(a) => format!("ActionCompleted:{}", a.label),
            build_event_id::Id::TargetCompleted(t) => format!("TargetCompleted:{}", t.label),
            build_event_id::Id::TestResult(t) => format!("TestResult:{}", t.label),
            build_event_id::Id::TestSummary(t) => format!("TestSummary:{}", t.label),
            build_event_id::Id::Pattern(p) => format!("Pattern:{}", p.pattern.join(",")),
            build_event_id::Id::PatternSkipped(p) => {
                format!("PatternSkipped:{}", p.pattern.join(","))
            }
            build_event_id::Id::TargetConfigured(t) => format!("TargetConfigured:{}", t.label),
            build_event_id::Id::Configuration(_) => "Configuration".to_string(),
            build_event_id::Id::NamedSet(_) => "NamedSetOfFiles".to_string(),
            build_event_id::Id::WorkspaceStatus(_) => "WorkspaceStatus".to_string(),
            build_event_id::Id::OptionsParsed(_) => "OptionsParsed".to_string(),
            build_event_id::Id::BuildMetadata(_) => "BuildMetadata".to_string(),
            build_event_id::Id::UnstructuredCommandLine(_) => "UnstructuredCommandLine".to_string(),
            build_event_id::Id::StructuredCommandLine(_) => "StructuredCommandLine".to_string(),
            build_event_id::Id::BuildToolLogs(_) => "BuildToolLogs".to_string(),
            build_event_id::Id::BuildMetrics(_) => "BuildMetrics".to_string(),
            build_event_id::Id::Fetch(_) => "Fetch".to_string(),
            build_event_id::Id::TargetSummary(_) => "TargetSummary".to_string(),
            build_event_id::Id::ConvenienceSymlinksIdentified(_) => {
                "ConvenienceSymlinksIdentified".to_string()
            }
            build_event_id::Id::ExecRequest(_) => "ExecRequest".to_string(),
            build_event_id::Id::TestProgress(_) => "TestProgress".to_string(),
            build_event_id::Id::SkyvalueUploaded(_) => "SkyValueUploaded".to_string(),
            build_event_id::Id::ConfiguredLabel(_) => "ConfiguredLabel".to_string(),
            build_event_id::Id::UnconfiguredLabel(_) => "UnconfiguredLabel".to_string(),
            other => format!("{:?}", other),
        }
    } else {
        "Unknown".to_string()
    }
}

fn target_label_from_id(id: &BuildEventId) -> Option<String> {
    id.id.as_ref().and_then(|inner| match inner {
        build_event_id::Id::TargetCompleted(t) => Some(t.label.clone()),
        build_event_id::Id::TargetConfigured(t) => Some(t.label.clone()),
        build_event_id::Id::TestResult(t) => Some(t.label.clone()),
        build_event_id::Id::TestSummary(t) => Some(t.label.clone()),
        _ => None,
    })
}

fn build_target_execution(
    label: &str,
    target_kind: &str,
    invocation_id: &str,
    completed: &Value,
    summary: &BuildSummary,
    metrics: &TargetActionMetrics,
    env_vars: &std::collections::HashMap<String, String>,
) -> TargetExecution {
    let success = completed
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let status = if success {
        TargetExecutionStatus::Success
    } else {
        TargetExecutionStatus::Failure
    };
    let tags = completed
        .get("tag")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    TargetExecution {
        label: label.to_string(),
        target_kind: target_kind.to_string(),
        invocation_id: invocation_id.to_string(),
        status,
        tags,
        build_start_time: summary.start_time.clone(),
        build_end_time: summary.end_time.clone(),
        build_duration_ms: summary.duration_ms,
        action_duration_ms: metrics.duration_ms,
        action_count: metrics.action_count,
        cached_actions: metrics.cached_actions,
        failed_actions: metrics.failed_actions,
        env_vars: env_vars.clone(),
    }
}

fn build_test_execution_json(label: &str, invocation_id: &str, result: &Value) -> TestExecution {
    let status = result
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("NO_STATUS")
        .to_string();
    let cached_locally = result
        .get("cached_locally")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let cached_remotely = result
        .get("cached_remotely")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    TestExecution {
        label: label.to_string(),
        invocation_id: invocation_id.to_string(),
        status,
        cached_locally,
        cached_remotely,
        start_time: None,
        end_time: None,
        duration_ms: 0,
    }
}

fn target_label_from_string_id(id: &str) -> Option<String> {
    if let Some(rest) = id.strip_prefix("TargetCompleted:") {
        return Some(rest.to_string());
    }
    if let Some(rest) = id.strip_prefix("TargetConfigured:") {
        return Some(rest.to_string());
    }
    if let Some(rest) = id.strip_prefix("TestResult:") {
        return Some(rest.to_string());
    }
    if let Some(rest) = id.strip_prefix("TestSummary:") {
        return Some(rest.to_string());
    }
    None
}

fn extract_env_vars(events: &[Value]) -> std::collections::HashMap<String, String> {
    let mut client_env = std::collections::HashMap::new();
    let mut explicit_env = std::collections::HashMap::new();

    fn extract_from_arg<'a>(s: &'a str, prefix: &str) -> Option<(&'a str, Option<&'a str>)> {
        let rest = s.rsplit_once(prefix).map(|(_, r)| r)?;
        Some(match rest.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (rest, None),
        })
    }

    for event in events {
        let Some(payload) = event.get("payload") else {
            continue;
        };
        let Some(args) = payload
            .get("UnstructuredCommandLine")
            .and_then(|u| u.get("args"))
            .and_then(|a| a.as_array())
        else {
            continue;
        };
        for arg in args {
            let Some(s) = arg.as_str() else { continue };
            if let Some((key, value)) = extract_from_arg(s, "--client_env=") {
                if let Some(value) = value {
                    client_env.insert(key.to_string(), value.to_string());
                }
            }
        }
        for arg in args {
            let Some(s) = arg.as_str() else { continue };
            for prefix in ["--action_env=", "--test_env=", "--repo_env="] {
                if let Some((key, value)) = extract_from_arg(s, prefix) {
                    let value = value
                        .map(|v| v.to_string())
                        .or_else(|| client_env.get(key).cloned())
                        .unwrap_or_default();
                    explicit_env.insert(key.to_string(), value);
                }
            }
        }
    }
    explicit_env
}

fn update_action_metrics(
    metrics: &mut HashMap<String, TargetActionMetrics>,
    action: &ActionExecuted,
) {
    let Some(label) = target_label_from_action_id(action) else {
        return;
    };
    if label.is_empty() {
        return;
    }

    let success = action.success;
    let start = action
        .start_time
        .as_ref()
        .and_then(|s| DateTime::parse_from_rfc3339(&prost_timestamp_to_rfc3339(s)).ok());
    let end = action
        .end_time
        .as_ref()
        .and_then(|s| DateTime::parse_from_rfc3339(&prost_timestamp_to_rfc3339(s)).ok());
    let duration_ms = match (start, end) {
        (Some(s), Some(e)) => (e - s).num_milliseconds() as u64,
        _ => 0,
    };

    // Bazel reports real cache hits inside SpawnExec strategy_details.
    // Fallback to the instant-execution heuristic when timestamps are present.
    let mut is_cached = false;
    for any in &action.strategy_details {
        if any.type_url.ends_with("SpawnExec") || any.type_url.contains("/SpawnExec") {
            if let Ok(spawn) = SpawnExec::decode(&*any.value) {
                is_cached = spawn.cache_hit;
                break;
            }
        }
    }
    if !is_cached {
        is_cached = success && duration_ms == 0 && start.is_some();
    }

    let entry = metrics.entry(label).or_default();
    entry.action_count += 1;
    entry.duration_ms += duration_ms;
    if is_cached {
        entry.cached_actions += 1;
    }
    if !success {
        entry.failed_actions += 1;
    }
}

fn target_label_from_action_id(action: &ActionExecuted) -> Option<String> {
    // The deprecated label field is still populated by current Bazel versions.
    if !action.label.is_empty() {
        return Some(action.label.clone());
    }
    None
}

fn build_event_id_to_json(id: &BuildEventId) -> Value {
    let mut obj = serde_json::json!({});
    if let Some(inner) = &id.id {
        match inner {
            build_event_id::Id::Unknown(u) => {
                obj["unknown"] = Value::String(u.details.clone());
            }
            build_event_id::Id::Progress(p) => {
                obj["progress"] = serde_json::json!({ "opaque_count": p.opaque_count });
            }
            build_event_id::Id::Started(_) => {
                obj["started"] = Value::Object(Default::default());
            }
            build_event_id::Id::BuildFinished(_) => {
                obj["build_finished"] = Value::Object(Default::default());
            }
            build_event_id::Id::ActionCompleted(a) => {
                obj["action_completed"] = serde_json::json!({
                    "label": a.label,
                    "primary_output": a.primary_output,
                });
            }
            build_event_id::Id::TargetCompleted(t) => {
                obj["target_completed"] = serde_json::json!({ "label": t.label });
            }
            build_event_id::Id::TestResult(t) => {
                obj["test_result"] = serde_json::json!({ "label": t.label });
            }
            build_event_id::Id::TestSummary(t) => {
                obj["test_summary"] = serde_json::json!({ "label": t.label });
            }
            build_event_id::Id::Pattern(p) => {
                obj["pattern"] =
                    Value::Array(p.pattern.iter().cloned().map(Value::String).collect());
            }
            build_event_id::Id::PatternSkipped(p) => {
                obj["pattern_skipped"] =
                    Value::Array(p.pattern.iter().cloned().map(Value::String).collect());
            }
            _ => {
                obj["other"] = Value::String(format!("{:?}", inner));
            }
        }
    }
    obj
}
