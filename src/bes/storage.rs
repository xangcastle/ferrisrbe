//! BES event storage and summary derivation.
//!
//! Memory is bounded by what is live: builds still streaming events, one
//! listing entry per retained build and one aggregate per known target or
//! test label. Everything per build (events, console, targets) lives on disk
//! under `<data_dir>/<invocation_id>/` and is read on demand.
//!
//! Several BEP fields read here, such as `ActionExecuted.label`, are marked
//! deprecated in the protocol yet remain the only source of that data in the
//! events Bazel emits, hence the module-wide `allow(deprecated)`.
#![allow(deprecated)]

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use prost::Message;
use serde::Deserialize;
use serde_json::Value;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

use crate::bes::config::BesConfig;
use crate::bes::console::Console;
use crate::bes::live::{LiveBus, LiveEvent};
use crate::bes::models::{
    BuildInputs, BuildListItem, BuildPage, BuildStatus, BuildSummary, GlobalStats,
    MnemonicTimeline, Page, TargetActionMetrics, TargetCacheMiss, TargetExecution,
    TargetExecutionStatus, TargetSummary as TargetSummaryModel, TestExecution,
    TestSummary as TestSummaryModel,
};
use crate::bes::profile::BuildProfile;
use crate::cache::action_cache_log::{ActionCacheLog, TargetCacheStats, WorkerPlacements};
use crate::metrics::Metrics;
use crate::proto::build::bazel::build_event_stream::build_event::Payload;
use crate::proto::build::bazel::build_event_stream::{
    build_event_id, ActionExecuted, BuildEvent, BuildEventId, BuildFinished, BuildMetrics,
    BuildStarted, BuildToolLogs, TargetComplete, TestResult, TestStatus,
};
use crate::proto::tools::protos::SpawnExec;
use std::collections::BTreeMap;
use tokio::task::JoinError;

/// Page size of the paginated listings when the caller gives none.
pub const DEFAULT_PAGE_SIZE: usize = 50;
/// Upper bound on the page size of every paginated listing.
pub const MAX_PAGE_SIZE: usize = 500;

/// A finalized invocation stays in memory this long so the trailing events
/// Bazel sends after `BuildFinished` (tool logs, `last_message`) find it.
const FINALIZED_GRACE: Duration = Duration::from_secs(300);
/// An invocation without events for this long is persisted and evicted.
const IDLE_EVICTION: Duration = Duration::from_secs(12 * 3600);
/// How long an empty build directory is left alone before the index drops
/// it, so one created by a stream that is about to write is kept.
const EMPTY_DIR_GRACE: Duration = Duration::from_secs(3600);
/// How long the event log of an in-progress build that is not in memory
/// stays unchanged before the build counts as interrupted; past Bazel's
/// retries of a stream cut by a server restart.
const ORPHAN_GRACE: Duration = Duration::from_secs(3600);
/// How often the action counts of running builds are refreshed.
const LIVE_COUNTERS_INTERVAL: Duration = Duration::from_secs(2);
/// Largest `BuildToolLogs` text kept inline in the event log.
const MAX_INLINE_TOOL_LOG: usize = 64 * 1024;
/// Period of the eviction sweep.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// Minimum time between two retention passes, and the delay before the first.
const RETENTION_INTERVAL: Duration = Duration::from_secs(6 * 3600);

/// `build_event_stream.TestStatus` value that [`is_important_event`] omits:
/// `PASSED`, like the successful actions and targets it leaves out.
const OMITTED_TEST_STATUS: i64 = 1;

/// Failure of a BES storage operation.
#[derive(Debug, thiserror::Error)]
pub enum BesError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Invocation not found: {0}")]
    InvocationNotFound(String),
}

/// Result of a BES storage operation.
pub type Result<T> = std::result::Result<T, BesError>;

/// In-memory state of an invocation that is still streaming or within its
/// finalization grace.
#[derive(Debug, Clone)]
struct InvocationState {
    summary: BuildSummary,
    /// Labels already listed in `summary.targets`.
    known_targets: HashSet<String>,
    events: Vec<Value>,
    /// Action totals per target label, joined into its execution record.
    action_metrics: HashMap<String, TargetActionMetrics>,
    /// The summary, console and target executions have been persisted.
    indexed: bool,
    /// When the build finished; starts the [`FINALIZED_GRACE`].
    finalized_at: Option<Instant>,
    last_event_at: Instant,
    /// Rule kind per target label, from `TargetConfigured`.
    target_kinds: HashMap<String, String>,
    console: Console,
    /// `BuildMetrics` arrived: its action counts are final and the live
    /// ones from the ActionCache no longer apply.
    has_metrics: bool,
}

impl InvocationState {
    fn new(invocation_id: &str) -> Self {
        Self {
            summary: BuildSummary {
                invocation_id: invocation_id.to_string(),
                ..Default::default()
            },
            known_targets: HashSet::new(),
            events: Vec::new(),
            action_metrics: HashMap::new(),
            indexed: false,
            finalized_at: None,
            last_event_at: Instant::now(),
            target_kinds: HashMap::new(),
            console: Console::default(),
            has_metrics: false,
        }
    }
}

/// Running totals of one target label across builds.
#[derive(Debug, Clone, Default)]
struct TargetAggregate {
    kind: String,
    latest_status: Option<TargetExecutionStatus>,
    latest_start: Option<String>,
    total: usize,
    success: usize,
    failure: usize,
    cached: usize,
    action_count: u64,
    cached_actions: u64,
    failed_actions: u64,
    timed: u64,
    total_action_ms: u64,
    max_action_ms: u64,
    tags: Vec<String>,
}

impl TargetAggregate {
    fn add(&mut self, ex: &TargetExecution) {
        self.total += 1;
        match ex.status {
            TargetExecutionStatus::Success => self.success += 1,
            TargetExecutionStatus::Failure => self.failure += 1,
            TargetExecutionStatus::Cached => self.cached += 1,
            TargetExecutionStatus::Dependency => {}
        }
        self.action_count += ex.action_count;
        self.cached_actions += ex.cached_actions;
        self.failed_actions += ex.failed_actions;
        if ex.action_duration_ms > 0 && !replayed_action_time(ex) {
            self.timed += 1;
            self.total_action_ms += ex.action_duration_ms;
            self.max_action_ms = self.max_action_ms.max(ex.action_duration_ms);
        }
        if self.kind.is_empty() && !ex.target_kind.is_empty() {
            self.kind = ex.target_kind.clone();
        }
        for tag in &ex.tags {
            if !self.tags.contains(tag) {
                self.tags.push(tag.clone());
            }
        }
        if self.latest_start.as_deref() <= ex.build_start_time.as_deref() {
            self.latest_start = ex.build_start_time.clone();
            self.latest_status = Some(ex.status);
        }
    }

    fn summary(&self, label: &str) -> TargetSummaryModel {
        TargetSummaryModel {
            label: label.to_string(),
            target_kind: self.kind.clone(),
            latest_status: self.latest_status.unwrap_or(TargetExecutionStatus::Success),
            total_executions: self.total,
            success_count: self.success,
            failure_count: self.failure,
            cached_count: self.cached,
            action_count: self.action_count,
            cached_actions: self.cached_actions,
            failed_actions: self.failed_actions,
            total_action_ms: self.total_action_ms,
            max_action_ms: self.max_action_ms,
            mean_action_ms: self.total_action_ms.checked_div(self.timed).unwrap_or(0),
            tags: self.tags.clone(),
        }
    }
}

/// Whether `ex` is a record from before [`action_duration_ms`] that counted
/// the replayed times of a cached test: one target's actions outlasting the
/// whole build.
fn replayed_action_time(ex: &TargetExecution) -> bool {
    ex.build_duration_ms > 0 && ex.action_duration_ms > ex.build_duration_ms
}

/// Running totals of one test label across builds.
#[derive(Debug, Clone, Default)]
struct TestAggregate {
    latest_status: Option<String>,
    latest_start: Option<String>,
    total: usize,
    passed: usize,
    failed: usize,
    cached: usize,
    flaky: usize,
    no_status: usize,
}

impl TestAggregate {
    fn add(&mut self, ex: &TestExecution) {
        self.total += 1;
        match ex.status.as_str() {
            "PASSED" => self.passed += 1,
            "FLAKY" => self.flaky += 1,
            "FAILED"
            | "TIMEOUT"
            | "INCOMPLETE"
            | "REMOTE_FAILURE"
            | "FAILED_TO_BUILD"
            | "TOOL_HALTED_BEFORE_TESTING" => self.failed += 1,
            "" | "NO_STATUS" => self.no_status += 1,
            _ => {}
        }
        if ex.cached_locally || ex.cached_remotely {
            self.cached += 1;
        }
        if self.latest_start.as_deref() <= ex.start_time.as_deref() {
            self.latest_start = ex.start_time.clone();
            self.latest_status = Some(ex.status.clone());
        }
    }

    fn summary(&self, label: &str) -> TestSummaryModel {
        TestSummaryModel {
            label: label.to_string(),
            latest_status: self
                .latest_status
                .clone()
                .unwrap_or_else(|| "NO_STATUS".to_string()),
            total_runs: self.total,
            passed_count: self.passed,
            failed_count: self.failed,
            cached_count: self.cached,
            flaky_count: self.flaky,
            no_status_count: self.no_status,
        }
    }
}

/// Persistent storage for BES invocations.
///
/// Cheap to clone: every clone shares the same state.
#[derive(Debug, Clone)]
pub struct BesStorage {
    config: BesConfig,
    /// Builds still receiving events, plus [`FINALIZED_GRACE`] after they
    /// finish.
    invocations: Arc<DashMap<String, InvocationState>>,
    /// Every retained build, as the listing shows it.
    index: Arc<DashMap<String, BuildListItem>>,
    /// Running totals per target label over the global target history.
    target_stats: Arc<DashMap<String, TargetAggregate>>,
    /// Running totals per test label over the global test history.
    test_stats: Arc<DashMap<String, TestAggregate>>,
    /// Set once the per-label histories hold every record of the global
    /// histories. While unset, label lookups scan the global files.
    label_index_ready: Arc<AtomicBool>,
    action_cache_log: Option<Arc<ActionCacheLog>>,
    live: Option<LiveBus>,
    metrics: Option<Arc<Metrics>>,
    /// Serializes every JSONL append and every rewrite of the histories.
    ///
    /// Every publishing build appends to the same global files; each record
    /// is written with a single `write_all` under this lock, so records never
    /// interleave. A target or test execution is appended to its global and
    /// its per-label history, and folded into its aggregate, under one
    /// acquisition. Compaction and aggregate reloads hold it for their whole
    /// duration, so no append lands between a clear and a rescan.
    jsonl_lock: Arc<tokio::sync::Mutex<()>>,
}

/// Parses a JSONL line as a stream of concatenated values and returns every
/// complete record, plus the error that ended the stream, if any. A line
/// holding several concatenated objects (`{...}{...}`) yields each of them.
fn salvage_line<T: serde::de::DeserializeOwned>(line: &str) -> (Vec<T>, Option<serde_json::Error>) {
    let stream = serde_json::Deserializer::from_str(line).into_iter::<T>();
    let mut recovered = Vec::new();
    for item in stream {
        match item {
            Ok(value) => recovered.push(value),
            Err(e) => return (recovered, Some(e)),
        }
    }
    (recovered, None)
}

/// Visits every record of a JSONL file, streaming it. A missing file has no
/// records; unparsable records are logged and skipped.
fn for_each_record<T, F>(path: &Path, visit: F) -> std::io::Result<()>
where
    T: serde::de::DeserializeOwned,
    F: FnMut(T),
{
    for_each_record_upto(path, None, visit)
}

/// [`for_each_record`] limited to the lines that end within the first
/// `limit` bytes of the file, when a limit is given.
fn for_each_record_upto<T, F>(path: &Path, limit: Option<u64>, mut visit: F) -> std::io::Result<()>
where
    T: serde::de::DeserializeOwned,
    F: FnMut(T),
{
    for_each_line_upto(path, limit, |line| {
        let (records, error) = salvage_line::<T>(line);
        for record in records {
            visit(record);
        }
        if let Some(e) = error {
            warn!("Skipping unparsable record in {}: {}", path.display(), e);
        }
    })
}

/// Visits every non-blank, trimmed line of a file, streaming it, up to the
/// lines that end within the first `limit` bytes when a limit is given. A
/// missing file has no lines.
fn for_each_line_upto<F>(path: &Path, limit: Option<u64>, mut visit: F) -> std::io::Result<()>
where
    F: FnMut(&str),
{
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let reader = BufReader::with_capacity(1 << 20, file);
    let mut consumed: u64 = 0;
    for line in reader.lines() {
        let line = line?;
        consumed += line.len() as u64 + 1;
        if limit.is_some_and(|max| consumed > max) {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        visit(line);
    }
    Ok(())
}

/// The fields of a stored event that decide whether it is important,
/// deserialized without building the full JSON tree of the line.
#[derive(Deserialize, Default)]
struct EventHead {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    payload: Option<PayloadHead>,
}

/// The payload fields [`EventHead`] needs.
#[derive(Deserialize, Default)]
struct PayloadHead {
    #[serde(rename = "ActionExecuted", default)]
    action: Option<SuccessHead>,
    #[serde(rename = "Completed", default)]
    completed: Option<SuccessHead>,
    #[serde(rename = "TestResult", default)]
    test: Option<TestStatusHead>,
}

/// A payload's `success` flag.
#[derive(Deserialize, Default)]
struct SuccessHead {
    #[serde(default)]
    success: bool,
}

/// A `TestResult` payload's `status`.
#[derive(Deserialize, Default)]
struct TestStatusHead {
    #[serde(default)]
    status: i64,
}

/// The rule of [`is_important_event`], decided from the event head alone.
fn is_important_head(head: &EventHead) -> bool {
    let payload = head.payload.as_ref();
    match head.kind.as_str() {
        "Progress"
        | "NamedSetOfFiles"
        | "TargetConfigured"
        | "Configuration"
        | "ConvenienceSymlinksIdentified"
        | "Fetch"
        | "WorkspaceInfo" => false,
        "ActionExecuted" => !payload
            .and_then(|p| p.action.as_ref())
            .map(|a| a.success)
            .unwrap_or(false),
        "TargetComplete" => !payload
            .and_then(|p| p.completed.as_ref())
            .map(|c| c.success)
            .unwrap_or(false),
        "TestResult" => {
            payload
                .and_then(|p| p.test.as_ref())
                .map(|t| t.status)
                .unwrap_or(0)
                != OMITTED_TEST_STATUS
        }
        _ => true,
    }
}

/// Splits the global histories into one file per label, copying the records
/// within the first `targets_limit` and `tests_limit` bytes, then writes the
/// sentinel. Returns the number of records copied.
///
/// Records appended past those limits are already written to their label
/// files by the appender; copying them again would duplicate them.
fn migrate_label_index(
    config: &BesConfig,
    targets_limit: u64,
    tests_limit: u64,
) -> std::io::Result<usize> {
    std::fs::create_dir_all(config.target_labels_dir())?;
    std::fs::create_dir_all(config.test_labels_dir())?;
    let mut writer = LabelWriter::default();
    for_each_line_upto(&config.targets_path(), Some(targets_limit), |line| {
        migrate_line(&mut writer, line, |label| config.target_label_path(label));
    })?;
    writer.flush()?;
    for_each_line_upto(&config.tests_path(), Some(tests_limit), |line| {
        migrate_line(&mut writer, line, |label| config.test_label_path(label));
    })?;
    writer.flush()?;
    std::fs::write(config.label_index_sentinel(), b"")?;
    Ok(writer.migrated)
}

/// Routes the records of one global-history line to their label files.
///
/// A line holding one record is forwarded verbatim. A line holding several
/// concatenated records is split with the same rule `for_each_record` applies
/// when reading (the records up to the first unparsable one), and each record
/// is re-serialized, so the per-label history holds exactly what a scan of the
/// global history would return.
fn migrate_line(writer: &mut LabelWriter, line: &str, label_path: impl Fn(&str) -> PathBuf) {
    if let Ok(head) = serde_json::from_str::<LabelHead>(line) {
        writer.push(label_path(&head.label), line);
        return;
    }
    for record in serde_json::Deserializer::from_str(line)
        .into_iter::<Value>()
        .map_while(std::result::Result::ok)
    {
        if let Some(label) = record.get("label").and_then(Value::as_str) {
            writer.push(label_path(label), &record.to_string());
        }
    }
}

/// Buffers records per label file and appends them in batches of up to
/// [`Self::FLUSH_BYTES`], so each file is opened once per batch rather than
/// once per record.
#[derive(Default)]
struct LabelWriter {
    buffers: HashMap<PathBuf, Vec<u8>>,
    buffered: usize,
    migrated: usize,
}

impl LabelWriter {
    const FLUSH_BYTES: usize = 64 << 20;

    fn push(&mut self, path: PathBuf, line: &str) {
        let buffer = self.buffers.entry(path).or_default();
        buffer.extend_from_slice(line.as_bytes());
        buffer.push(b'\n');
        self.buffered += line.len() + 1;
        self.migrated += 1;
        if self.buffered >= Self::FLUSH_BYTES {
            if let Err(e) = self.flush() {
                warn!("label index flush failed: {}", e);
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.buffered = 0;
        for (path, data) in self.buffers.drain() {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            file.write_all(&data)?;
        }
        Ok(())
    }
}

/// The `label` field of a history record.
#[derive(Deserialize)]
struct LabelHead {
    label: String,
}

/// Every record of `path` that `keep` accepts, read on the blocking pool.
async fn read_records<T, F>(path: PathBuf, keep: F) -> Result<Vec<T>>
where
    T: serde::de::DeserializeOwned + Send + 'static,
    F: Fn(&T) -> bool + Send + 'static,
{
    let records = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<T>> {
        let mut out = Vec::new();
        for_each_record::<T, _>(&path, |record| {
            if keep(&record) {
                out.push(record);
            }
        })?;
        Ok(out)
    })
    .await
    .map_err(join_error)??;
    Ok(records)
}

/// Writes `records` as JSON lines to `path` through a sibling temporary
/// file, so a concurrent reader sees the old file or the whole new one.
async fn write_records_atomically<T: serde::Serialize>(path: &Path, records: &[T]) -> Result<()> {
    let mut data = Vec::new();
    for record in records {
        serde_json::to_writer(&mut data, record)?;
        data.push(b'\n');
    }
    let tmp = path.with_extension("jsonl.tmp");
    fs::write(&tmp, data).await?;
    fs::rename(&tmp, path).await?;
    Ok(())
}

/// The files of `dir` when it holds nothing but empty files and has not
/// changed for [`EMPTY_DIR_GRACE`]: a build directory no event was ever
/// written to, as opposed to one a stream just created. `None` otherwise.
async fn stale_hollow_dir(dir: &Path) -> Option<Vec<PathBuf>> {
    let age = fs::metadata(dir)
        .await
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()?;
    if age < EMPTY_DIR_GRACE {
        return None;
    }
    let mut entries = fs::read_dir(dir).await.ok()?;
    let mut files = Vec::new();
    while let Some(entry) = entries.next_entry().await.ok()? {
        let meta = entry.metadata().await.ok()?;
        if !meta.is_file() || meta.len() > 0 {
            return None;
        }
        files.push(entry.path());
    }
    Some(files)
}

/// The fields of a stored event [`summary_from_event_log`] reads.
#[derive(Deserialize, Default)]
struct LogEvent {
    #[serde(default)]
    payload: Option<LogPayload>,
}

#[derive(Deserialize, Default)]
struct LogPayload {
    #[serde(rename = "Started", default)]
    started: Option<LogStarted>,
    #[serde(rename = "Finished", default)]
    finished: Option<LogFinished>,
    #[serde(rename = "ActionExecuted", default)]
    action: Option<SuccessHead>,
    #[serde(rename = "BuildMetadata", default)]
    metadata: Option<LogMetadata>,
}

#[derive(Deserialize, Default)]
struct LogStarted {
    #[serde(default)]
    command: String,
    #[serde(default)]
    workspace_directory: String,
    #[serde(default)]
    start_time: Option<String>,
}

#[derive(Deserialize, Default)]
struct LogFinished {
    #[serde(default)]
    overall_success: bool,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    finish_time: Option<String>,
}

#[derive(Deserialize, Default)]
struct LogMetadata {
    #[serde(default)]
    metadata: BTreeMap<String, String>,
}

/// The summary of a build rebuilt from its stored event log: command,
/// workspace, times, action counts, metadata, and the outcome of
/// `BuildFinished`, or [`BuildStatus::Interrupted`] without one.
fn summary_from_event_log(path: &Path, invocation_id: &str) -> std::io::Result<BuildSummary> {
    let mut summary = BuildSummary {
        invocation_id: invocation_id.to_string(),
        status: BuildStatus::Interrupted,
        ..Default::default()
    };
    for_each_line_upto(path, None, |line| {
        let Some(payload) = serde_json::from_str::<LogEvent>(line)
            .ok()
            .and_then(|e| e.payload)
        else {
            return;
        };
        if let Some(started) = payload.started {
            summary.command = started.command;
            summary.workspace = started.workspace_directory;
            summary.start_time = started.start_time;
        }
        if let Some(action) = payload.action {
            summary.total_actions += 1;
            if !action.success {
                summary.failed_actions += 1;
            }
        }
        if let Some(metadata) = payload.metadata {
            summary.metadata = metadata.metadata;
        }
        if let Some(finished) = payload.finished {
            summary.status = if finished.overall_success || finished.exit_code == Some(0) {
                BuildStatus::Success
            } else {
                BuildStatus::Failure
            };
            summary.end_time = finished.finish_time;
        }
    })?;
    Ok(summary)
}

/// The whole command line Bazel reports, unstructured, for `bazel run`: its
/// structured "original" command line redacts only the binary's arguments.
const BAZEL_REDACTED: &str = "REDACTED";

/// Collects [`BuildInputs`] from the stored events that carry them.
#[derive(Default)]
struct InputsReader {
    inputs: BuildInputs,
    unstructured: Vec<String>,
    original: Vec<String>,
    seen: HashSet<&'static str>,
}

impl InputsReader {
    const KINDS: [&'static str; 4] = [
        "UnstructuredCommandLine",
        "StructuredCommandLine",
        "OptionsParsed",
        "WorkspaceStatus",
    ];

    fn wants(kind: &str) -> bool {
        Self::KINDS.contains(&kind)
    }

    /// Takes what `event` carries; whether every input event has been seen.
    fn absorb(&mut self, event: &Value) -> bool {
        let kind = event.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        let Some(&kind) = Self::KINDS.iter().find(|k| **k == kind) else {
            return false;
        };
        let payload = &event["payload"][kind];
        match kind {
            "UnstructuredCommandLine" => self.unstructured = strings(payload, "args"),
            "StructuredCommandLine" => {
                if payload["command_line_label"].as_str() != Some("original") {
                    return false;
                }
                self.original = strings(payload, "args");
            }
            "OptionsParsed" => {
                self.inputs.options = strings(payload, "cmd_line");
                self.inputs.explicit_options = strings(payload, "explicit_cmd_line");
                self.inputs.startup_options = strings(payload, "startup_options");
            }
            _ => {
                for item in payload["item"].as_array().into_iter().flatten() {
                    if let (Some(k), Some(v)) = (item["key"].as_str(), item["value"].as_str()) {
                        self.inputs
                            .workspace_status
                            .insert(k.to_string(), v.to_string());
                    }
                }
            }
        }
        self.seen.insert(kind);
        self.seen.len() == Self::KINDS.len()
    }

    fn finish(mut self) -> BuildInputs {
        self.inputs.command_line =
            if self.unstructured == [BAZEL_REDACTED] && !self.original.is_empty() {
                self.original
            } else {
                self.unstructured
            };
        for arg in &self.inputs.command_line {
            if let Some((name, value)) = arg
                .strip_prefix("--client_env=")
                .and_then(|pair| pair.split_once('='))
            {
                self.inputs.env.insert(name.to_string(), value.to_string());
            }
        }
        self.inputs
    }
}

/// The strings of the array `key` of `payload`.
fn strings(payload: &Value, key: &str) -> Vec<String> {
    payload[key]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str().map(String::from))
        .collect()
}

/// [`BesStorage::get_build_inputs`] for a persisted build: parses in full
/// only the lines whose kind carries an input, redacted as served.
fn read_build_inputs(path: &Path) -> std::io::Result<BuildInputs> {
    let mut inputs = InputsReader::default();
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(inputs.finish()),
        Err(e) => return Err(e),
    };
    for line in BufReader::with_capacity(1 << 20, file).lines() {
        let line = line?;
        let head: EventHead = serde_json::from_str(&line).unwrap_or_default();
        if !InputsReader::wants(&head.kind) {
            continue;
        }
        let Ok(mut event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        super::redact::event(&mut event);
        if inputs.absorb(&event) {
            break;
        }
    }
    Ok(inputs.finish())
}

/// The CAS blob of the Bazel profile in a stored `BuildToolLogs` event.
fn profile_blob_of(event: &Value) -> Option<(String, i64)> {
    if event.get("kind").and_then(|k| k.as_str()) != Some("BuildToolLogs") {
        return None;
    }
    event["payload"]["BuildToolLogs"]["log"]
        .as_array()?
        .iter()
        .find(|log| {
            log["name"]
                .as_str()
                .is_some_and(|name| name.ends_with("profile.gz") || name.ends_with("profile.json"))
        })
        .and_then(|log| {
            let hash = log["digest"].as_str().filter(|h| !h.is_empty())?;
            Some((hash.to_string(), log["length"].as_i64()?))
        })
}

/// Writes `value` as JSON to `path` through a sibling temporary file.
async fn write_records_atomically_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(value)?).await?;
    fs::rename(&tmp, path).await?;
    Ok(())
}

/// A blocking task that panicked or was cancelled, as an I/O error.
fn join_error(error: JoinError) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

/// The `limit` items of `items` after the first `offset`, with the total
/// count. `limit` is clamped to `1..=MAX_PAGE_SIZE`.
fn paginate<T>(items: Vec<T>, limit: usize, offset: usize) -> Page<T> {
    let total = items.len();
    let items = items
        .into_iter()
        .skip(offset)
        .take(limit.clamp(1, MAX_PAGE_SIZE))
        .collect();
    Page { items, total }
}

/// Whether `query` is a case-insensitive substring of `label` or `kind`; an
/// empty query matches everything.
fn label_matches(label: &str, kind: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let query = query.to_lowercase();
    label.to_lowercase().contains(&query) || kind.to_lowercase().contains(&query)
}

/// Opaque listing cursor: the sort key of the last item served.
fn cursor_of(item: &BuildListItem) -> String {
    format!(
        "{}|{}",
        item.start_time.as_deref().unwrap_or(""),
        item.invocation_id
    )
}

/// Listing order key: start time, then invocation id. The listing sorts it
/// descending.
fn sort_key(item: &BuildListItem) -> (String, String) {
    (
        item.start_time.clone().unwrap_or_default(),
        item.invocation_id.clone(),
    )
}

/// Inverse of [`cursor_of`].
fn parse_cursor(cursor: &str) -> Option<(String, String)> {
    let (start, id) = cursor.rsplit_once('|')?;
    Some((start.to_string(), id.to_string()))
}

/// Parses an RFC 3339 timestamp into UTC; `None` for absent or invalid input.
fn parse_rfc3339(value: Option<&str>) -> Option<DateTime<Utc>> {
    value
        .and_then(|v| DateTime::parse_from_rfc3339(v).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

/// How far before its build an action may start and still count as run by
/// it, for clock skew between the client and the build start.
const ACTION_START_SKEW: chrono::Duration = chrono::Duration::minutes(1);

/// Wall time of an action that ran in the build started at `build_start`.
///
/// Bazel replays a cached test result with the `ActionExecuted` times of the
/// run that produced it, days before the build: such an action, or one
/// without both times, counts as zero.
fn action_duration_ms(
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    build_start: Option<DateTime<Utc>>,
) -> u64 {
    let (Some(start), Some(end)) = (start, end) else {
        return 0;
    };
    if build_start.is_some_and(|build| start < build - ACTION_START_SKEW) {
        return 0;
    }
    (end - start).num_milliseconds().max(0) as u64
}

impl BesStorage {
    /// Opens the store at `config.data_dir`, creating its directories.
    ///
    /// The listing index and the aggregates start empty; [`Self::warm_up`]
    /// loads them. Starts the per-label index migration when it has not
    /// completed.
    pub async fn new(config: BesConfig) -> Result<Self> {
        fs::create_dir_all(&config.data_dir).await?;
        let storage = Self {
            config,
            invocations: Arc::new(DashMap::new()),
            index: Arc::new(DashMap::new()),
            target_stats: Arc::new(DashMap::new()),
            test_stats: Arc::new(DashMap::new()),
            label_index_ready: Arc::new(AtomicBool::new(false)),
            action_cache_log: None,
            live: None,
            metrics: None,
            jsonl_lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        fs::create_dir_all(storage.config.target_labels_dir()).await?;
        fs::create_dir_all(storage.config.test_labels_dir()).await?;
        storage.ensure_label_index();
        Ok(storage)
    }

    /// Loads the listing index and the aggregates from disk.
    ///
    /// Safe to run while builds are streaming: an index entry written by a
    /// live build is kept, the scan only fills the builds it lacks.
    pub async fn warm_up(&self) -> Result<()> {
        self.load_index().await?;
        self.reload_aggregates().await
    }

    /// Marks the per-label index ready when its sentinel exists; otherwise
    /// migrates the global histories into it in the background.
    ///
    /// The migration copies only the bytes the global files hold when it
    /// starts: every later append writes its label file itself. The index is
    /// marked ready once the migration succeeds; on failure label lookups
    /// keep scanning the global files.
    fn ensure_label_index(&self) {
        if self.config.label_index_sentinel().exists() {
            self.label_index_ready.store(true, Ordering::Release);
            return;
        }
        let targets_limit = std::fs::metadata(self.config.targets_path())
            .map(|m| m.len())
            .unwrap_or(0);
        let tests_limit = std::fs::metadata(self.config.tests_path())
            .map(|m| m.len())
            .unwrap_or(0);
        let config = self.config.clone();
        let ready = self.label_index_ready.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let result = tokio::task::spawn_blocking(move || {
                migrate_label_index(&config, targets_limit, tests_limit)
            })
            .await;
            match result {
                Ok(Ok(records)) => {
                    ready.store(true, Ordering::Release);
                    info!(
                        "BES label index built: {} records in {:?}",
                        records,
                        started.elapsed()
                    );
                }
                Ok(Err(e)) => warn!("BES label index migration failed: {}", e),
                Err(e) => warn!("BES label index migration panicked: {}", e),
            }
        });
    }

    /// Waits up to five seconds for the per-label index to become ready.
    #[cfg(test)]
    async fn wait_label_index(&self) {
        for _ in 0..500 {
            if self.label_index_ready.load(Ordering::Acquire) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("label index never became ready");
    }

    /// Publishes build and target updates, console output and failed actions
    /// on `bus` as events arrive.
    pub fn with_live_bus(mut self, bus: LiveBus) -> Self {
        self.live = Some(bus);
        self
    }

    fn publish(&self, event: LiveEvent) {
        if let Some(bus) = &self.live {
            bus.publish(event);
        }
    }

    /// Joins the ActionCache hits and misses recorded in `log` into each
    /// finished build's target executions.
    pub fn with_action_cache_log(mut self, log: Arc<ActionCacheLog>) -> Self {
        self.action_cache_log = Some(log);
        self
    }

    /// Exports each finished build's status, duration and critical path.
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Starts the housekeeping task: every `MAINTENANCE_INTERVAL` it evicts
    /// finished and idle invocations from memory, and every
    /// `RETENTION_INTERVAL` it applies the configured retention.
    ///
    /// The first retention pass runs one full `RETENTION_INTERVAL` after
    /// the task starts. During a rolling update the new instance runs beside
    /// the one it replaces, which keeps appending to the histories a
    /// retention pass rewrites.
    pub fn spawn_maintenance(&self) {
        let storage = self.clone();
        tokio::spawn(async move {
            let mut last_retention = Instant::now();
            let mut interval = tokio::time::interval(MAINTENANCE_INTERVAL);
            loop {
                interval.tick().await;
                storage.sweep_invocations().await;
                storage.recover_orphans().await;
                if last_retention.elapsed() >= RETENTION_INTERVAL {
                    if let Err(e) = storage.apply_retention().await {
                        warn!("BES retention pass failed: {}", e);
                    }
                    last_retention = Instant::now();
                }
            }
        });
    }

    /// The ActionCache hits and misses recorded for a build, per target
    /// label, persisted to `cache.json` in its directory when not empty.
    async fn cache_stats(&self, invocation_id: &str) -> BTreeMap<String, TargetCacheStats> {
        let Some(log) = &self.action_cache_log else {
            return BTreeMap::new();
        };
        let stats = log.snapshot(invocation_id).await;
        if !stats.is_empty() {
            let path = self.config.invocation_dir(invocation_id).join("cache.json");
            match serde_json::to_string(&stats) {
                Ok(json) => {
                    if let Err(e) = fs::write(&path, json).await {
                        warn!("Failed to persist cache stats for {}: {}", invocation_id, e);
                    }
                }
                Err(e) => warn!(
                    "Failed to serialize cache stats for {}: {}",
                    invocation_id, e
                ),
            }
        }
        stats
    }

    /// Persists the worker placements recorded for a build to
    /// `placements.json` in its directory, when it has any.
    async fn persist_placements(&self, invocation_id: &str) {
        let Some(log) = &self.action_cache_log else {
            return;
        };
        let placements = log.placements(invocation_id).await;
        if placements.is_empty() {
            return;
        }
        let path = self
            .config
            .invocation_dir(invocation_id)
            .join("placements.json");
        match serde_json::to_vec(&placements) {
            Ok(json) => {
                if let Err(e) = fs::write(&path, json).await {
                    warn!("Failed to persist placements for {}: {}", invocation_id, e);
                }
            }
            Err(e) => warn!(
                "Failed to serialize placements for {}: {}",
                invocation_id, e
            ),
        }
    }

    /// Which worker produced each output of a build's remote executions:
    /// the persisted record, or the live one while the build runs.
    pub async fn get_placements(&self, invocation_id: &str) -> WorkerPlacements {
        let path = self
            .config
            .invocation_dir(invocation_id)
            .join("placements.json");
        match fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                warn!("Unreadable placements for {}: {}", invocation_id, e);
                WorkerPlacements::default()
            }),
            Err(_) => match &self.action_cache_log {
                Some(log) => log.placements(invocation_id).await,
                None => WorkerPlacements::default(),
            },
        }
    }

    /// Takes `jsonl_lock`, recording how long the wait took.
    async fn lock_jsonl(&self) -> tokio::sync::MutexGuard<'_, ()> {
        let waiting = std::time::Instant::now();
        let guard = self.jsonl_lock.lock().await;
        self.record_store("lock_wait", waiting);
        guard
    }

    fn record_store(&self, phase: &str, since: std::time::Instant) {
        if let Some(metrics) = &self.metrics {
            metrics.record_bes_store(phase, since.elapsed());
        }
    }

    /// Appends one newline-terminated record to `path` under `jsonl_lock`.
    async fn append_record(&self, path: &Path, record: &[u8]) -> Result<()> {
        let _jsonl = self.lock_jsonl().await;
        self.append_record_locked(path, record).await
    }

    /// Appends one newline-terminated record with a single write. The caller
    /// holds `jsonl_lock`.
    async fn append_record_locked(&self, path: &Path, record: &[u8]) -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .write(true)
            .open(path)
            .await?;
        file.write_all(record).await?;
        file.flush().await?;
        Ok(())
    }

    /// Records one build event: appends it to the invocation's event log,
    /// updates the in-memory summary and listing entry, and publishes the
    /// resulting live updates.
    ///
    /// On `BuildFinished` or `last_message` the build's summary, console and
    /// target executions are persisted once; `last_message` also evicts the
    /// build from memory. A `last_message` for a build already evicted, as
    /// when a stream is retried after the grace period, is only appended to
    /// the event log.
    ///
    /// The `invocations` entry guard is never held across an `.await`: the
    /// map's readers run on the same runtime threads.
    pub async fn store_event(&self, invocation_id: &str, event: BuildEvent) -> Result<()> {
        let started = std::time::Instant::now();
        let stored = self.store_event_timed(invocation_id, event).await;
        self.record_store("event", started);
        stored
    }

    async fn store_event_timed(&self, invocation_id: &str, event: BuildEvent) -> Result<()> {
        let event_json = build_event_to_json(&event);
        let is_final = event.last_message;

        fs::create_dir_all(self.config.invocation_dir(invocation_id)).await?;

        let mut record = serde_json::to_vec(&event_json)?;
        record.push(b'\n');
        self.append_record(&self.config.events_path(invocation_id), &record)
            .await?;

        if is_final && !self.invocations.contains_key(invocation_id) {
            return Ok(());
        }

        let is_finished = matches!(&event.payload, Some(Payload::Finished(_)));
        let index_now;
        {
            let mut entry = self
                .invocations
                .entry(invocation_id.to_string())
                .or_insert_with(|| InvocationState::new(invocation_id));
            let state = entry.value_mut();
            state.last_event_at = Instant::now();
            state.events.push(event_json.clone());
            update_summary(&mut state.summary, &mut state.known_targets, &event);
            if matches!(&event.payload, Some(Payload::BuildMetrics(_))) {
                state.has_metrics = true;
            }
            self.publish_deltas(invocation_id, state, &event, &event_json);

            match &event.payload {
                Some(Payload::Configured(configured)) => {
                    if let Some(id) = &event.id {
                        if let Some(label) = target_label_from_id(id) {
                            state
                                .target_kinds
                                .insert(label, configured.target_kind.clone());
                        }
                    }
                }
                Some(Payload::Action(action)) => {
                    let build_start = parse_rfc3339(state.summary.start_time.as_deref());
                    update_action_metrics(&mut state.action_metrics, action, build_start);
                }
                Some(Payload::BuildToolLogs(logs)) => {
                    if let (Some(metrics), Some(critical_path)) =
                        (&self.metrics, critical_path_of(logs))
                    {
                        metrics
                            .record_bes_critical_path(command_label(&state.summary), critical_path);
                    }
                }
                Some(Payload::Progress(progress)) => {
                    state.console.feed(&progress.stderr);
                    state.console.feed(&progress.stdout);
                    if let Some((from, lines)) = state.console.take_delta() {
                        self.publish(LiveEvent::LogAppended {
                            invocation_id: invocation_id.to_string(),
                            from,
                            lines,
                        });
                    }
                }
                _ => {}
            }

            if summary_changed(&event) {
                self.index.insert(
                    invocation_id.to_string(),
                    BuildListItem::from(&state.summary),
                );
            }

            index_now = (is_final || is_finished) && !state.indexed;
            if index_now && state.summary.status == BuildStatus::InProgress {
                close_unfinished(&mut state.summary);
                self.index.insert(
                    invocation_id.to_string(),
                    BuildListItem::from(&state.summary),
                );
            }
            if index_now {
                state.indexed = true;
                state.finalized_at = Some(Instant::now());
            }
        }

        if index_now {
            let snapshot = self
                .invocations
                .get(invocation_id)
                .map(|state| state.value().clone());
            if let Some(state) = snapshot {
                self.persist_finished(invocation_id, &state).await?;
            }
        }
        if is_final {
            self.evict(invocation_id).await?;
        }
        Ok(())
    }

    /// Writes the summary, console and target executions of a finished build,
    /// refreshes its listing entry and publishes its targets.
    async fn persist_finished(&self, invocation_id: &str, state: &InvocationState) -> Result<()> {
        let summary_json = serde_json::to_string_pretty(&state.summary)?;
        fs::write(self.config.summary_path(invocation_id), summary_json).await?;
        fs::write(self.console_path(invocation_id), state.console.render()).await?;
        self.index.insert(
            invocation_id.to_string(),
            BuildListItem::from(&state.summary),
        );
        if let Some(metrics) = &self.metrics {
            if let Some(status) = final_status_label(state.summary.status) {
                metrics.record_bes_build(
                    command_label(&state.summary),
                    status,
                    Duration::from_millis(state.summary.duration_ms),
                );
            }
        }
        let cache_stats = self.cache_stats(invocation_id).await;
        self.persist_placements(invocation_id).await;
        let targets = self
            .index_invocation_targets(
                invocation_id,
                &state.events,
                &state.target_kinds,
                &state.summary,
                &state.action_metrics,
                &cache_stats,
            )
            .await?;
        debug!("Finalized invocation {}", invocation_id);
        if self.live.is_some() {
            self.publish(LiveEvent::TargetsUpdated {
                invocation_id: invocation_id.to_string(),
                targets,
            });
        }
        Ok(())
    }

    /// Removes an invocation from memory, persisting everything not yet on
    /// disk and rewriting its summary with the last state seen.
    async fn evict(&self, invocation_id: &str) -> Result<()> {
        let Some((_, state)) = self.invocations.remove(invocation_id) else {
            return Ok(());
        };
        if !state.indexed {
            self.persist_finished(invocation_id, &state).await?;
        } else {
            let summary_json = serde_json::to_string_pretty(&state.summary)?;
            fs::write(self.config.summary_path(invocation_id), summary_json).await?;
        }
        Ok(())
    }

    /// Evicts builds past their [`FINALIZED_GRACE`] and builds silent for
    /// [`IDLE_EVICTION`].
    async fn sweep_invocations(&self) {
        let now = Instant::now();
        let stale: Vec<String> = self
            .invocations
            .iter()
            .filter(|entry| {
                let state = entry.value();
                match state.finalized_at {
                    Some(at) => now.duration_since(at) >= FINALIZED_GRACE,
                    None => now.duration_since(state.last_event_at) >= IDLE_EVICTION,
                }
            })
            .map(|entry| entry.key().clone())
            .collect();
        for invocation_id in stale {
            if let Err(e) = self.evict(&invocation_id).await {
                warn!("Failed to evict invocation {}: {}", invocation_id, e);
            }
        }
    }

    /// Starts the task that keeps the action counts of running builds
    /// current: Bazel reports them in `BuildMetrics` only when a build ends,
    /// and per action only with `--build_event_publish_all_actions`. A no-op
    /// without a live bus or an ActionCache log.
    pub fn spawn_live_counters(&self) {
        if self.live.is_none() || self.action_cache_log.is_none() {
            return;
        }
        let storage = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(LIVE_COUNTERS_INTERVAL);
            loop {
                interval.tick().await;
                storage.refresh_live_counters().await;
            }
        });
    }

    /// Sets each running build's action counts from the ActionCache lookups
    /// recorded for it (hits are cached actions, misses executed ones) and
    /// publishes the builds whose counts changed.
    async fn refresh_live_counters(&self) {
        let Some(log) = &self.action_cache_log else {
            return;
        };
        let running: Vec<String> = self
            .invocations
            .iter()
            .filter(|e| !e.value().has_metrics && e.value().finalized_at.is_none())
            .map(|e| e.key().clone())
            .collect();
        for invocation_id in running {
            let stats = log.snapshot(&invocation_id).await;
            let (hits, misses) = stats
                .values()
                .fold((0u64, 0u64), |(h, m), s| (h + s.hits, m + s.misses));
            let item = {
                let Some(mut entry) = self.invocations.get_mut(&invocation_id) else {
                    continue;
                };
                let state = entry.value_mut();
                let summary = &mut state.summary;
                let total = hits + misses;
                if state.has_metrics
                    || (summary.total_actions >= total
                        && summary.cached_actions == hits
                        && summary.local_actions == misses)
                {
                    continue;
                }
                summary.total_actions = summary.total_actions.max(total);
                summary.cached_actions = hits;
                summary.remote_cache_hits = hits;
                summary.local_actions = misses;
                BuildListItem::from(&*summary)
            };
            self.index.insert(invocation_id, item.clone());
            self.publish(LiveEvent::BuildUpdated {
                build: Box::new(item),
            });
        }
    }

    /// Closes the builds the listing shows in progress that no stream is
    /// feeding: rebuilds each summary from its event log and persists it.
    async fn recover_orphans(&self) {
        let candidates: Vec<String> = self
            .index
            .iter()
            .filter(|entry| {
                entry.value().status == BuildStatus::InProgress
                    && !self.invocations.contains_key(entry.key())
            })
            .map(|entry| entry.key().clone())
            .collect();
        let mut recovered = 0usize;
        for invocation_id in candidates {
            match self.recover_orphan(&invocation_id).await {
                Ok(true) => recovered += 1,
                Ok(false) => {}
                Err(e) => warn!("Failed to recover build {}: {}", invocation_id, e),
            }
        }
        if recovered > 0 {
            info!("BES recovered {} interrupted builds", recovered);
        }
    }

    /// Persists the summary of one orphaned build once its event log has
    /// been quiet for [`ORPHAN_GRACE`]. Whether it did.
    async fn recover_orphan(&self, invocation_id: &str) -> Result<bool> {
        let events_path = self.config.events_path(invocation_id);
        let Ok(modified) = fs::metadata(&events_path).await.and_then(|m| m.modified()) else {
            return Ok(false);
        };
        if modified.elapsed().map_or(true, |age| age < ORPHAN_GRACE) {
            return Ok(false);
        }
        let id = invocation_id.to_string();
        let mut summary =
            tokio::task::spawn_blocking(move || summary_from_event_log(&events_path, &id))
                .await
                .map_err(join_error)??;
        if summary.end_time.is_none() {
            summary.end_time = Some(DateTime::<Utc>::from(modified).to_rfc3339());
        }
        if let (Some(start), Some(end)) = (
            parse_rfc3339(summary.start_time.as_deref()),
            parse_rfc3339(summary.end_time.as_deref()),
        ) {
            summary.duration_ms = (end - start).num_milliseconds().max(0) as u64;
        }
        if self.invocations.contains_key(invocation_id) {
            return Ok(false);
        }
        fs::write(
            self.config.summary_path(invocation_id),
            serde_json::to_string_pretty(&summary)?,
        )
        .await?;
        let item = BuildListItem::from(&summary);
        self.index.insert(invocation_id.to_string(), item.clone());
        self.publish(LiveEvent::BuildUpdated {
            build: Box::new(item),
        });
        Ok(true)
    }

    /// Publishes the live updates an event causes: summary changes, target
    /// completions and failed actions. Console output is published where the
    /// console is fed.
    fn publish_deltas(
        &self,
        invocation_id: &str,
        entry: &InvocationState,
        event: &BuildEvent,
        event_json: &Value,
    ) {
        if self.live.is_none() {
            return;
        }
        match &event.payload {
            Some(Payload::Started(_))
            | Some(Payload::Expanded(_))
            | Some(Payload::BuildMetadata(_))
            | Some(Payload::BuildMetrics(_))
            | Some(Payload::Finished(_)) => {
                self.publish(LiveEvent::BuildUpdated {
                    build: Box::new(BuildListItem::from(&entry.summary)),
                });
            }
            Some(Payload::Completed(completed)) => {
                if let Some(label) = event.id.as_ref().and_then(target_label_from_id) {
                    self.publish(LiveEvent::TargetCompleted {
                        invocation_id: invocation_id.to_string(),
                        label,
                        success: completed.success,
                    });
                }
            }
            Some(Payload::Action(action)) if !action.success => {
                if let Some(action) = event_json
                    .get("payload")
                    .and_then(|p| p.get("ActionExecuted"))
                {
                    self.publish(LiveEvent::ActionFailed {
                        invocation_id: invocation_id.to_string(),
                        action: action.clone(),
                    });
                }
            }
            _ => {}
        }
    }

    /// Persists whatever a build reported and releases it from memory. Called
    /// when its event stream ends.
    pub async fn finalize(&self, invocation_id: &str) -> Result<()> {
        self.evict(invocation_id).await
    }

    /// Every retained build, newest first, from the in-memory index.
    pub async fn list_builds(&self) -> Result<Vec<BuildListItem>> {
        let mut builds: Vec<BuildListItem> = self
            .index
            .iter()
            .map(|entry| entry.value().clone())
            .collect();
        builds.sort_by_key(|b| std::cmp::Reverse(sort_key(b)));
        Ok(builds)
    }

    /// One page of the builds matching `filter`, newest first.
    ///
    /// `cursor` is the `next_cursor` of the previous page; the page after it
    /// starts at the first build that sorts below it, so builds added since
    /// never repeat an item. `limit` is clamped to `1..=MAX_PAGE_SIZE`.
    pub async fn list_builds_page(
        &self,
        limit: usize,
        cursor: Option<&str>,
        filter: impl Fn(&BuildListItem) -> bool,
    ) -> Result<BuildPage> {
        let limit = limit.clamp(1, MAX_PAGE_SIZE);
        let after = cursor.and_then(parse_cursor);
        let all = self.list_builds().await?;
        let matching: Vec<BuildListItem> = all.into_iter().filter(|b| filter(b)).collect();
        let total = matching.len();
        let start = match &after {
            Some(key) => matching
                .iter()
                .position(|b| sort_key(b) < *key)
                .unwrap_or(total),
            None => 0,
        };
        let page: Vec<BuildListItem> = matching.into_iter().skip(start).take(limit).collect();
        let next_cursor = if start + page.len() < total {
            page.last().map(cursor_of)
        } else {
            None
        };
        Ok(BuildPage {
            builds: page,
            next_cursor,
            total,
        })
    }

    /// The summary of one build, live or persisted; `None` when unknown.
    pub async fn get_build(&self, invocation_id: &str) -> Result<Option<BuildSummary>> {
        if let Some(entry) = self.invocations.get(invocation_id) {
            return Ok(Some(entry.summary.clone()));
        }
        if !self.config.invocation_dir(invocation_id).exists() {
            return Ok(None);
        }
        self.load_summary_from_disk(invocation_id).await.map(Some)
    }

    /// The action executions of one build aggregated per mnemonic, ranked by
    /// total action time, then by mnemonic.
    ///
    /// Reads every event of the build through [`Self::get_events`], so a
    /// running build and a persisted one are answered alike. Actions without
    /// a mnemonic are grouped as `unknown`.
    pub async fn get_action_timeline(&self, invocation_id: &str) -> Result<Vec<MnemonicTimeline>> {
        let events = self.get_events(invocation_id, false).await?;
        let mut by_mnemonic: HashMap<String, MnemonicTimeline> = HashMap::new();
        let build_start = events
            .iter()
            .find_map(|event| parse_rfc3339(event["payload"]["Started"]["start_time"].as_str()));

        for event in &events {
            if event.get("kind").and_then(Value::as_str) != Some("ActionExecuted") {
                continue;
            }
            let Some(action) = event.get("payload").and_then(|p| p.get("ActionExecuted")) else {
                continue;
            };
            let mnemonic = action
                .get("type")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("unknown")
                .to_string();
            let label = action.get("label").and_then(Value::as_str).unwrap_or("");
            let success = action
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let start = parse_rfc3339(action.get("start_time").and_then(Value::as_str));
            let end = parse_rfc3339(action.get("end_time").and_then(Value::as_str));
            let duration_ms = action_duration_ms(start, end, build_start);

            let entry = by_mnemonic
                .entry(mnemonic.clone())
                .or_insert_with(|| MnemonicTimeline {
                    mnemonic,
                    ..Default::default()
                });
            entry.action_count += 1;
            entry.total_duration_ms += duration_ms;
            if !success {
                entry.failed_count += 1;
            }
            if let Some(s) = start {
                let s = s.to_rfc3339();
                if entry
                    .min_start
                    .as_deref()
                    .is_none_or(|min| s.as_str() < min)
                {
                    entry.min_start = Some(s);
                }
            }
            if let Some(e) = end {
                let e = e.to_rfc3339();
                if entry.max_end.as_deref().is_none_or(|max| e.as_str() > max) {
                    entry.max_end = Some(e);
                }
            }
            if duration_ms > entry.longest_duration_ms {
                entry.longest_duration_ms = duration_ms;
                entry.longest_label = (!label.is_empty()).then(|| label.to_string());
            }
        }

        let mut timeline: Vec<MnemonicTimeline> = by_mnemonic.into_values().collect();
        timeline.sort_by(|a, b| {
            b.total_duration_ms
                .cmp(&a.total_duration_ms)
                .then_with(|| a.mnemonic.cmp(&b.mnemonic))
        });
        Ok(timeline)
    }

    /// The events of one build as stored, oldest first. With
    /// `important_only`, only those `is_important_event` keeps; from disk
    /// the filter runs per line before a line is parsed in full.
    pub async fn get_events(
        &self,
        invocation_id: &str,
        important_only: bool,
    ) -> Result<Vec<Value>> {
        if let Some(entry) = self.invocations.get(invocation_id) {
            let events = entry.events.clone();
            drop(entry);
            if important_only {
                return Ok(events.into_iter().filter(is_important_event).collect());
            }
            return Ok(events);
        }
        let mut events = if important_only {
            self.load_important_events_from_disk(invocation_id).await?
        } else {
            self.load_events_from_disk(invocation_id).await?
        };
        events.iter_mut().for_each(super::redact::event);
        Ok(events)
    }

    /// The command line, options, client environment and workspace status a
    /// build reported, or `None` for an unknown build. Reads only until the
    /// three events that carry them are found.
    pub async fn get_build_inputs(&self, invocation_id: &str) -> Result<Option<BuildInputs>> {
        if let Some(entry) = self.invocations.get(invocation_id) {
            let mut inputs = InputsReader::default();
            for event in &entry.events {
                if inputs.absorb(event) {
                    break;
                }
            }
            return Ok(Some(inputs.finish()));
        }
        if !self.config.invocation_dir(invocation_id).exists() {
            return Ok(None);
        }
        let path = self.config.events_path(invocation_id);
        let inputs = tokio::task::spawn_blocking(move || read_build_inputs(&path))
            .await
            .map_err(join_error)??;
        Ok(Some(inputs))
    }

    /// The CAS blob (hash, size) of the Bazel profile a build announced in
    /// `BuildToolLogs`, or `None` when it announced none. Reads only the
    /// `BuildToolLogs` events.
    pub async fn get_profile_blob(&self, invocation_id: &str) -> Result<Option<(String, i64)>> {
        if let Some(entry) = self.invocations.get(invocation_id) {
            return Ok(entry.events.iter().rev().find_map(profile_blob_of));
        }
        let path = self.config.events_path(invocation_id);
        let found =
            tokio::task::spawn_blocking(move || -> std::io::Result<Option<(String, i64)>> {
                let mut found = None;
                for_each_line_upto(&path, None, |line| {
                    let head: EventHead = serde_json::from_str(line).unwrap_or_default();
                    if head.kind != "BuildToolLogs" {
                        return;
                    }
                    if let Ok(event) = serde_json::from_str::<Value>(line) {
                        found = profile_blob_of(&event).or(found.take());
                    }
                })?;
                Ok(found)
            })
            .await
            .map_err(join_error)??;
        Ok(found)
    }

    /// The profile summary saved for a finished build, if any.
    pub async fn get_saved_profile(&self, invocation_id: &str) -> Result<Option<BuildProfile>> {
        match fs::read(
            self.config
                .invocation_dir(invocation_id)
                .join("profile.json"),
        )
        .await
        {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Saves the profile summary of a build that is no longer running, so
    /// later requests skip the CAS read and the parse.
    pub async fn save_profile(&self, invocation_id: &str, profile: &BuildProfile) -> Result<()> {
        if self.invocations.contains_key(invocation_id) {
            return Ok(());
        }
        let path = self
            .config
            .invocation_dir(invocation_id)
            .join("profile.json");
        write_records_atomically_json(&path, profile).await
    }

    /// Rendered console of a persisted build.
    fn console_path(&self, invocation_id: &str) -> PathBuf {
        self.config
            .invocation_dir(invocation_id)
            .join("console.log")
    }

    /// Bazel's console output of one build, as a terminal shows it at the
    /// end; empty when none was recorded.
    pub async fn get_console(&self, invocation_id: &str) -> Result<String> {
        if let Some(entry) = self.invocations.get(invocation_id) {
            return Ok(entry.console.render());
        }
        match fs::read_to_string(self.console_path(invocation_id)).await {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// Actions the ActionCache could not serve for this build, by target and
    /// mnemonic, most misses first.
    pub async fn get_misses(&self, invocation_id: &str) -> Result<Vec<TargetCacheMiss>> {
        let mut misses: Vec<TargetCacheMiss> = self
            .get_build_targets(invocation_id)
            .await?
            .into_iter()
            .flat_map(|t| {
                t.missed_mnemonics
                    .iter()
                    .map(|(mnemonic, count)| TargetCacheMiss {
                        label: t.label.clone(),
                        mnemonic: mnemonic.clone(),
                        misses: *count,
                        hits: t.cache_hits,
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        misses.sort_by(|a, b| b.misses.cmp(&a.misses).then_with(|| a.label.cmp(&b.label)));
        Ok(misses)
    }

    /// Appends a target execution to the global and the per-label history and
    /// folds it into its aggregate, all under one `jsonl_lock` acquisition.
    async fn append_target_execution(&self, execution: &TargetExecution) -> Result<()> {
        let _jsonl = self.lock_jsonl().await;
        let mut record = serde_json::to_vec(execution)?;
        record.push(b'\n');
        self.append_record_locked(&self.config.targets_path(), &record)
            .await?;
        self.append_record_locked(&self.config.target_label_path(&execution.label), &record)
            .await?;
        self.target_stats
            .entry(execution.label.clone())
            .or_default()
            .add(execution);
        Ok(())
    }

    /// Appends a test execution to the global and the per-label history and
    /// folds it into its aggregate, all under one `jsonl_lock` acquisition.
    async fn append_test_execution(&self, execution: &TestExecution) -> Result<()> {
        let _jsonl = self.lock_jsonl().await;
        let mut record = serde_json::to_vec(execution)?;
        record.push(b'\n');
        self.append_record_locked(&self.config.tests_path(), &record)
            .await?;
        self.append_record_locked(&self.config.test_label_path(&execution.label), &record)
            .await?;
        self.test_stats
            .entry(execution.label.clone())
            .or_default()
            .add(execution);
        Ok(())
    }

    /// Adds every persisted build missing from the listing index.
    async fn load_index(&self) -> Result<()> {
        let mut entries = fs::read_dir(&self.config.data_dir).await?;
        let mut loaded = 0usize;
        let mut removed = 0usize;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if !self.config.is_invocation_dir(&path) {
                continue;
            }
            let Some(invocation_id) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !self.invocations.contains_key(invocation_id) {
                if let Some(files) = stale_hollow_dir(&path).await {
                    for file in files {
                        let _ = fs::remove_file(file).await;
                    }
                    if fs::remove_dir(&path).await.is_ok() {
                        removed += 1;
                    }
                    continue;
                }
            }
            let summary = self.load_summary_from_disk(invocation_id).await?;
            self.index
                .entry(invocation_id.to_string())
                .or_insert_with(|| BuildListItem::from(&summary));
            loaded += 1;
        }
        info!(
            "BES index loaded: {} builds, {} empty directories removed",
            loaded, removed
        );
        Ok(())
    }

    /// Rebuilds the per-label aggregates from the global histories. Holds
    /// `jsonl_lock` throughout, so no append lands between the clear and the
    /// rescan to be counted twice or not at all.
    async fn reload_aggregates(&self) -> Result<()> {
        let _jsonl = self.lock_jsonl().await;
        let held = std::time::Instant::now();
        let reloaded = self.reload_aggregates_locked().await;
        self.record_store("maintenance", held);
        reloaded
    }

    /// [`Self::reload_aggregates`] for a caller that holds `jsonl_lock`.
    async fn reload_aggregates_locked(&self) -> Result<()> {
        let targets_path = self.config.targets_path();
        let tests_path = self.config.tests_path();
        let target_stats = self.target_stats.clone();
        let test_stats = self.test_stats.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            target_stats.clear();
            test_stats.clear();
            for_each_record::<TargetExecution, _>(&targets_path, |ex| {
                target_stats.entry(ex.label.clone()).or_default().add(&ex);
            })?;
            for_each_record::<TestExecution, _>(&tests_path, |ex| {
                test_stats.entry(ex.label.clone()).or_default().add(&ex);
            })?;
            Ok(())
        })
        .await
        .map_err(join_error)??;
        info!(
            "BES aggregates loaded: {} targets, {} tests",
            self.target_stats.len(),
            self.test_stats.len()
        );
        Ok(())
    }

    /// Every known target with its aggregate, most executed first.
    pub async fn list_targets(&self) -> Result<Vec<TargetSummaryModel>> {
        let mut summaries: Vec<TargetSummaryModel> = self
            .target_stats
            .iter()
            .map(|entry| entry.value().summary(entry.key()))
            .collect();
        summaries.sort_by(|a, b| {
            b.total_executions
                .cmp(&a.total_executions)
                .then_with(|| a.label.cmp(&b.label))
        });
        Ok(summaries)
    }

    /// One page of [`Self::list_targets`], keeping the targets whose label or
    /// kind contains `query` (case-insensitive) and, when given, whose latest
    /// status is `status`. `limit` is clamped to `1..=MAX_PAGE_SIZE`.
    pub async fn list_targets_page(
        &self,
        limit: usize,
        offset: usize,
        query: &str,
        status: Option<TargetExecutionStatus>,
    ) -> Result<Page<TargetSummaryModel>> {
        let matching: Vec<TargetSummaryModel> = self
            .list_targets()
            .await?
            .into_iter()
            .filter(|t| label_matches(&t.label, &t.target_kind, query))
            .filter(|t| status.is_none_or(|s| t.latest_status == s))
            .collect();
        Ok(paginate(matching, limit, offset))
    }

    /// One page of [`Self::list_tests`], keeping the tests whose label
    /// contains `query` (case-insensitive). `limit` is clamped to
    /// `1..=MAX_PAGE_SIZE`.
    pub async fn list_tests_page(
        &self,
        limit: usize,
        offset: usize,
        query: &str,
    ) -> Result<Page<TestSummaryModel>> {
        let matching: Vec<TestSummaryModel> = self
            .list_tests()
            .await?
            .into_iter()
            .filter(|t| label_matches(&t.label, "", query))
            .collect();
        Ok(paginate(matching, limit, offset))
    }

    /// Target executions of one build, sorted by label: from the build's own
    /// targets file. Empty for an unknown build and for one still running,
    /// whose file is written when it finishes.
    ///
    /// A build kept from before per-build files is selected once from the
    /// global history, which spans every retained build, and saved as its
    /// own file for the next read.
    pub async fn get_build_targets(&self, invocation_id: &str) -> Result<Vec<TargetExecution>> {
        let own = self.config.invocation_targets_path(invocation_id);
        let mut targets: Vec<TargetExecution> = if own.exists() {
            read_records(own, |_: &TargetExecution| true).await?
        } else if self.invocations.contains_key(invocation_id)
            || !self.config.invocation_dir(invocation_id).exists()
        {
            Vec::new()
        } else {
            let wanted = invocation_id.to_string();
            let targets = read_records(self.config.targets_path(), move |ex: &TargetExecution| {
                ex.invocation_id == wanted
            })
            .await?;
            let mut targets = targets;
            for target in &mut targets {
                super::redact::env(&mut target.env_vars);
            }
            let has_events = self.config.events_path(invocation_id).exists();
            let saved = if has_events || !targets.is_empty() {
                write_records_atomically(&own, &targets).await
            } else {
                Ok(())
            };
            if let Err(e) = saved {
                warn!("Could not save the targets of {invocation_id}: {e}");
            }
            targets
        };
        targets.sort_by(|a, b| a.label.cmp(&b.label));
        for target in &mut targets {
            super::redact::env(&mut target.env_vars);
        }
        Ok(targets)
    }

    /// Every retained execution of one target, oldest build first: read from
    /// its label file once the per-label index is ready, from the global
    /// history until then.
    pub async fn get_target_history(&self, label: &str) -> Result<Vec<TargetExecution>> {
        if !self.target_stats.contains_key(label) {
            return Ok(Vec::new());
        }
        let indexed = self.label_index_ready.load(Ordering::Acquire);
        let path = if indexed {
            self.config.target_label_path(label)
        } else {
            self.config.targets_path()
        };
        let wanted = label.to_string();
        let mut history = read_records(path, move |ex: &TargetExecution| {
            indexed || ex.label == wanted
        })
        .await?;
        history.sort_by(|a, b| {
            a.build_start_time
                .cmp(&b.build_start_time)
                .then_with(|| a.invocation_id.cmp(&b.invocation_id))
        });
        for execution in &mut history {
            super::redact::env(&mut execution.env_vars);
        }
        Ok(history)
    }

    /// Every known test with its aggregate, most run first.
    pub async fn list_tests(&self) -> Result<Vec<TestSummaryModel>> {
        let mut summaries: Vec<TestSummaryModel> = self
            .test_stats
            .iter()
            .map(|entry| entry.value().summary(entry.key()))
            .collect();
        summaries.sort_by(|a, b| {
            b.total_runs
                .cmp(&a.total_runs)
                .then_with(|| a.label.cmp(&b.label))
        });
        Ok(summaries)
    }

    /// Every retained run of one test, oldest first: read from its label file
    /// once the per-label index is ready, from the global history until then.
    pub async fn get_test_history(&self, label: &str) -> Result<Vec<TestExecution>> {
        if !self.test_stats.contains_key(label) {
            return Ok(Vec::new());
        }
        let indexed = self.label_index_ready.load(Ordering::Acquire);
        let path = if indexed {
            self.config.test_label_path(label)
        } else {
            self.config.tests_path()
        };
        let wanted = label.to_string();
        let mut history = read_records(path, move |ex: &TestExecution| {
            indexed || ex.label == wanted
        })
        .await?;
        history.sort_by(|a, b| {
            a.start_time
                .cmp(&b.start_time)
                .then_with(|| a.invocation_id.cmp(&b.invocation_id))
        });
        Ok(history)
    }

    /// Totals over every retained build, from the listing index.
    pub async fn stats(&self) -> Result<GlobalStats> {
        let mut stats = GlobalStats {
            total_builds: 0,
            in_progress_builds: 0,
            successful_builds: 0,
            failed_builds: 0,
            interrupted_builds: 0,
            total_actions: 0,
            failed_actions: 0,
        };
        for entry in self.index.iter() {
            let build = entry.value();
            stats.total_builds += 1;
            match build.status {
                BuildStatus::InProgress => stats.in_progress_builds += 1,
                BuildStatus::Success => stats.successful_builds += 1,
                BuildStatus::Failure => stats.failed_builds += 1,
                BuildStatus::Interrupted => stats.interrupted_builds += 1,
            }
            stats.total_actions += build.total_actions;
            stats.failed_actions += build.failed_actions;
        }
        Ok(stats)
    }

    /// Deletes every build not in memory that ended (or, lacking an end,
    /// started) before the retention cutoff, then compacts the histories to
    /// the builds that remain. A build with no recorded time is aged by its
    /// directory's modification time. Returns the number deleted; a no-op
    /// without a configured retention.
    pub async fn apply_retention(&self) -> Result<usize> {
        let Some(days) = self.config.max_retention_days else {
            return Ok(0);
        };
        let cutoff = Utc::now() - chrono::Duration::days(i64::from(days));
        let mut entries = fs::read_dir(&self.config.data_dir).await?;
        let mut removed = 0usize;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if !self.config.is_invocation_dir(&path) {
                continue;
            }
            let Some(invocation_id) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if self.invocations.contains_key(invocation_id) {
                continue;
            }
            let mut age_marker = self
                .index
                .get(invocation_id)
                .and_then(|b| parse_rfc3339(b.end_time.as_deref().or(b.start_time.as_deref())));
            if age_marker.is_none() {
                age_marker = entry
                    .metadata()
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .map(DateTime::<Utc>::from);
            }
            let Some(when) = age_marker else {
                continue;
            };
            if when >= cutoff {
                continue;
            }
            fs::remove_dir_all(&path).await?;
            self.index.remove(invocation_id);
            removed += 1;
        }
        if removed > 0 {
            info!(
                "BES retention: removed {} builds older than {} days",
                removed, days
            );
            self.compact_histories().await?;
        }
        Ok(removed)
    }

    /// Rewrites the global and, once ready, the per-label histories to the
    /// retained builds, then rebuilds the aggregates, all under `jsonl_lock`.
    async fn compact_histories(&self) -> Result<()> {
        let retained: Arc<HashSet<String>> =
            Arc::new(self.index.iter().map(|entry| entry.key().clone()).collect());
        let _jsonl = self.lock_jsonl().await;
        let held = std::time::Instant::now();
        for path in [self.config.targets_path(), self.config.tests_path()] {
            let retained = retained.clone();
            tokio::task::spawn_blocking(move || compact_jsonl(&path, &retained))
                .await
                .map_err(join_error)??;
        }
        if self.label_index_ready.load(Ordering::Acquire) {
            for dir in [
                self.config.target_labels_dir(),
                self.config.test_labels_dir(),
            ] {
                let retained = retained.clone();
                tokio::task::spawn_blocking(move || compact_label_dir(&dir, &retained))
                    .await
                    .map_err(join_error)??;
            }
        }
        let compacted = self.reload_aggregates_locked().await;
        self.record_store("maintenance", held);
        compacted
    }

    /// Derives the target and test executions of a finished build, writes its
    /// own targets file and appends them to the histories. Returns the
    /// target executions sorted by label.
    ///
    /// Bazel reports `TargetComplete` only for requested targets; every other
    /// target the ActionCache served is recorded with status `Dependency`.
    async fn index_invocation_targets(
        &self,
        invocation_id: &str,
        events: &[Value],
        target_kinds: &HashMap<String, String>,
        summary: &BuildSummary,
        action_metrics: &HashMap<String, TargetActionMetrics>,
        cache_stats: &BTreeMap<String, TargetCacheStats>,
    ) -> Result<Vec<TargetExecution>> {
        let env_vars = extract_env_vars(events);
        let mut completed_labels: HashSet<String> = HashSet::new();
        let mut target_index: HashMap<String, usize> = HashMap::new();
        let mut targets: Vec<TargetExecution> = Vec::new();
        let mut tests: Vec<TestExecution> = Vec::new();

        for event_json in events {
            let payload = event_json.get("payload");
            let id = event_json.get("id").and_then(|v| v.as_str());

            if let (Some(payload), Some(id_str)) = (payload, id) {
                if let Some(completed) = payload.get("Completed") {
                    if let Some(label) = target_label_from_string_id(id_str) {
                        if let Some(&at) = target_index.get(&label) {
                            let failed = !completed
                                .get("success")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            if failed {
                                targets[at].status = TargetExecutionStatus::Failure;
                            }
                            continue;
                        }
                        let target_kind = target_kinds.get(&label).cloned().unwrap_or_default();
                        let metrics = action_metrics.get(&label).cloned().unwrap_or_default();
                        let mut execution = build_target_execution(
                            &label,
                            &target_kind,
                            invocation_id,
                            completed,
                            summary,
                            &metrics,
                            &env_vars,
                        );
                        if let Some(stats) = cache_stats.get(&label) {
                            execution.cache_hits = stats.hits;
                            execution.cache_misses = stats.misses;
                            execution.missed_mnemonics = stats.missed_mnemonics.clone();
                            execution.heaviest_inputs = stats.heaviest_inputs.clone();
                        }
                        completed_labels.insert(label.clone());
                        target_index.insert(label, targets.len());
                        targets.push(execution);
                    }
                }

                if let Some(result) = payload.get("TestResult") {
                    if let Some(label) = target_label_from_string_id(id_str) {
                        tests.push(build_test_execution_json(&label, invocation_id, result));
                    }
                }
            }
        }

        for (label, stats) in cache_stats {
            if completed_labels.contains(label) {
                continue;
            }
            let metrics = action_metrics.get(label).cloned().unwrap_or_default();
            targets.push(TargetExecution {
                label: label.clone(),
                target_kind: target_kinds.get(label).cloned().unwrap_or_default(),
                invocation_id: invocation_id.to_string(),
                status: TargetExecutionStatus::Dependency,
                tags: Vec::new(),
                build_start_time: summary.start_time.clone(),
                build_end_time: summary.end_time.clone(),
                build_duration_ms: summary.duration_ms,
                action_duration_ms: metrics.duration_ms,
                action_count: metrics.action_count,
                cached_actions: metrics.cached_actions,
                failed_actions: metrics.failed_actions,
                env_vars: env_vars.clone(),
                cache_hits: stats.hits,
                cache_misses: stats.misses,
                missed_mnemonics: stats.missed_mnemonics.clone(),
                heaviest_inputs: stats.heaviest_inputs.clone(),
            });
        }

        let mut own = Vec::new();
        for execution in &targets {
            own.extend(serde_json::to_vec(execution)?);
            own.push(b'\n');
        }
        fs::write(self.config.invocation_targets_path(invocation_id), own).await?;
        for execution in &targets {
            self.append_target_execution(execution).await?;
        }
        for execution in &tests {
            self.append_test_execution(execution).await?;
        }
        targets.sort_by(|a, b| a.label.cmp(&b.label));
        Ok(targets)
    }

    /// The persisted summary of a build, or an empty one carrying only its id.
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

    /// The important events of a persisted build. Each line is first decoded
    /// as an [`EventHead`]; only the lines that pass are parsed in full.
    async fn load_important_events_from_disk(&self, invocation_id: &str) -> Result<Vec<Value>> {
        let events_path = self.config.events_path(invocation_id);
        if !events_path.exists() {
            return Ok(Vec::new());
        }
        let events = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<Value>> {
            let mut out = Vec::new();
            for_each_line_upto(&events_path, None, |line| {
                let head: EventHead = serde_json::from_str(line).unwrap_or_default();
                if !is_important_head(&head) {
                    return;
                }
                let (records, _) = salvage_line::<Value>(line);
                out.extend(records.into_iter().filter(is_important_event));
            })?;
            Ok(out)
        })
        .await
        .map_err(join_error)??;
        Ok(events)
    }

    /// Every event of a persisted build.
    async fn load_events_from_disk(&self, invocation_id: &str) -> Result<Vec<Value>> {
        let events_path = self.config.events_path(invocation_id);
        if !events_path.exists() {
            return Ok(Vec::new());
        }
        read_records(events_path, |_: &Value| true).await
    }
}

/// Compacts every per-label history of a directory, deleting the files left
/// empty.
fn compact_label_dir(dir: &Path, retained: &HashSet<String>) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        compact_jsonl(&path, retained)?;
        if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) == 0 {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Rewrites a history file with only the records whose top-level
/// `invocation_id` is in `retained`, replacing it atomically by rename.
fn compact_jsonl(path: &Path, retained: &HashSet<String>) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let tmp = path.with_extension("jsonl.tmp");
    let mut out = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&tmp)?);
    let mut kept = 0usize;
    let mut dropped = 0usize;
    for_each_record::<Value, _>(path, |record| {
        let keep = record
            .get("invocation_id")
            .and_then(|v| v.as_str())
            .map(|id| retained.contains(id))
            .unwrap_or(false);
        if keep {
            if serde_json::to_writer(&mut out, &record).is_ok() && out.write_all(b"\n").is_ok() {
                kept += 1;
            }
        } else {
            dropped += 1;
        }
    })?;
    out.flush()?;
    drop(out);
    std::fs::rename(&tmp, path)?;
    if dropped > 0 {
        debug!(
            "BES compaction of {}: kept {} records, dropped {}",
            path.display(),
            kept,
            dropped
        );
    }
    Ok(())
}

/// Whether an event changes the build's listing entry.
fn summary_changed(event: &BuildEvent) -> bool {
    matches!(
        &event.payload,
        Some(Payload::Started(_))
            | Some(Payload::Expanded(_))
            | Some(Payload::BuildMetadata(_))
            | Some(Payload::BuildMetrics(_))
            | Some(Payload::Finished(_))
    ) || event
        .id
        .as_ref()
        .is_some_and(|id| matches!(id.id, Some(build_event_id::Id::Pattern(_))))
}

/// Undoes Bazel's double encoding of build metadata.
///
/// Bazel holds argv as Latin-1 and re-encodes it as UTF-8 when serializing
/// the BEP, so `José` arrives as `JosÃ©`. A value whose characters all fit
/// in Latin-1 and whose bytes then decode as non-ASCII UTF-8 is decoded;
/// anything else is returned unchanged.
fn repair_mojibake(value: &str) -> String {
    if value.is_ascii() || value.chars().any(|c| c as u32 > 0xFF) {
        return value.to_string();
    }
    let bytes: Vec<u8> = value.chars().map(|c| c as u32 as u8).collect();
    match String::from_utf8(bytes) {
        Ok(decoded) if !decoded.is_ascii() => decoded,
        _ => value.to_string(),
    }
}

/// [`repair_mojibake`] of each of `values`.
fn repair_all(values: &[String]) -> Vec<String> {
    values.iter().map(|v| repair_mojibake(v)).collect()
}

/// The arguments of a structured command line in order: its chunks and the
/// combined form of its options, as Bazel would print them.
fn structured_args(
    command_line: &crate::proto::build::bazel::command_line::CommandLine,
) -> Vec<String> {
    use crate::proto::build::bazel::command_line::command_line_section::SectionType;
    command_line
        .sections
        .iter()
        .flat_map(|section| match &section.section_type {
            Some(SectionType::ChunkList(chunks)) => chunks.chunk.clone(),
            Some(SectionType::OptionList(options)) => options
                .option
                .iter()
                .map(|o| o.combined_form.clone())
                .filter(|form| !form.is_empty())
                .collect(),
            None => Vec::new(),
        })
        .map(|arg| repair_mojibake(&arg))
        .collect()
}

/// Whether a stored event is worth reading by hand: what was run, what
/// failed and how it ended.
///
/// Omits progress, file sets, configuration and workspace events, successful
/// actions and targets, and test results with [`OMITTED_TEST_STATUS`].
fn is_important_event(event: &Value) -> bool {
    let kind = event.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    let payload = event.get("payload");
    let succeeded = |key: &str| {
        payload
            .and_then(|p| p.get(key))
            .and_then(|p| p.get("success"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    };
    match kind {
        "Progress"
        | "NamedSetOfFiles"
        | "TargetConfigured"
        | "Configuration"
        | "ConvenienceSymlinksIdentified"
        | "Fetch"
        | "WorkspaceInfo" => false,
        "ActionExecuted" => !succeeded("ActionExecuted"),
        "TargetComplete" => !succeeded("Completed"),
        "TestResult" => {
            let status = payload
                .and_then(|p| p.get("TestResult"))
                .and_then(|p| p.get("status"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            status != OMITTED_TEST_STATUS
        }
        _ => true,
    }
}

/// Folds one BEP event into a build summary.
fn update_summary(
    summary: &mut BuildSummary,
    known_targets: &mut HashSet<String>,
    event: &BuildEvent,
) {
    let event_id = event.id.as_ref();
    if let Some(id) = event_id {
        match &id.id {
            Some(build_event_id::Id::Started(_)) => summary.status = BuildStatus::InProgress,
            Some(build_event_id::Id::Pattern(p)) => summary.pattern = p.pattern.clone(),
            _ => {}
        }
    }

    if let Some(payload) = &event.payload {
        match payload {
            Payload::Started(started) => handle_started(summary, started),
            Payload::Finished(finished) => handle_finished(summary, finished),
            Payload::Action(action) => handle_action(summary, known_targets, event_id, action),
            Payload::Completed(completed) => {
                handle_completed(summary, known_targets, event_id, completed)
            }
            Payload::BuildMetrics(metrics) => handle_build_metrics(summary, metrics),
            Payload::BuildMetadata(m) => {
                summary.metadata = m
                    .metadata
                    .iter()
                    .map(|(k, v)| (k.clone(), repair_mojibake(v)))
                    .collect()
            }
            _ => {}
        }
    }
}

/// Command, workspace and start time.
fn handle_started(summary: &mut BuildSummary, started: &BuildStarted) {
    summary.command = started.command.clone();
    summary.workspace = started.workspace_directory.clone();
    #[allow(deprecated)]
    let millis = started.start_time_millis;
    summary.start_time = match &started.start_time {
        Some(ts) => Some(prost_timestamp_to_rfc3339(ts)),
        None if millis > 0 => DateTime::from_timestamp_millis(millis).map(|dt| dt.to_rfc3339()),
        None => summary.start_time.take(),
    };
}

/// A build whose stream ended without `BuildFinished`, as when Bazel
/// aborts before building: interrupted, ending now.
fn close_unfinished(summary: &mut BuildSummary) {
    let now = Utc::now();
    summary.status = BuildStatus::Interrupted;
    summary.end_time = Some(now.to_rfc3339());
    if let Some(start) = parse_rfc3339(summary.start_time.as_deref()) {
        summary.duration_ms = (now - start).num_milliseconds().max(0) as u64;
    }
}

/// Final status, end time and duration; the failure detail of a failed build.
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
            summary.errors.push(failure_message(detail));
        }
    }
}

/// Action totals, and the action's target in the build's target list.
fn handle_action(
    summary: &mut BuildSummary,
    known_targets: &mut HashSet<String>,
    event_id: Option<&BuildEventId>,
    action: &ActionExecuted,
) {
    summary.total_actions += 1;
    if let Some(id) = event_id {
        if let Some(build_event_id::Id::ActionCompleted(a)) = &id.id {
            if !a.label.is_empty() && known_targets.insert(a.label.clone()) {
                summary.targets.push(a.label.clone());
            }
        }
    }
    if !action.success {
        summary.failed_actions += 1;
    }
}

/// The completed target in the build's target list; its failure detail.
fn handle_completed(
    summary: &mut BuildSummary,
    known_targets: &mut HashSet<String>,
    event_id: Option<&BuildEventId>,
    completed: &TargetComplete,
) {
    if let Some(id) = event_id {
        if let Some(build_event_id::Id::TargetCompleted(t)) = &id.id {
            if !t.label.is_empty() && known_targets.insert(t.label.clone()) {
                summary.targets.push(t.label.clone());
            }
        }
    }
    if !completed.success {
        if let Some(detail) = &completed.failure_detail {
            summary.errors.push(failure_message(detail));
        }
    }
}

/// Action totals and the split between cache hits and executed spawns.
///
/// Bazel reports one `RunnerCount` per spawn strategy, named as in its
/// "N processes: ..." summary line. Cache hits are runners too, so that line
/// is the only source of the split.
fn handle_build_metrics(summary: &mut BuildSummary, metrics: &BuildMetrics) {
    let Some(action_summary) = &metrics.action_summary else {
        return;
    };
    summary.total_actions = action_summary.actions_executed.max(0) as u64;
    summary.remote_cache_hits = 0;
    summary.cached_actions = 0;
    summary.local_actions = 0;
    summary.runners.clear();
    for runner in &action_summary.runner_count {
        let count = runner.count.max(0) as u64;
        if runner.name != "total" && count > 0 {
            *summary.runners.entry(runner.name.clone()).or_default() += count;
        }
        match runner.name.as_str() {
            "total" => summary.total_actions = count,
            "internal" => {}
            "remote cache hit" => {
                summary.remote_cache_hits += count;
                summary.cached_actions += count;
            }
            name if name.ends_with(" cache hit") => summary.cached_actions += count,
            _ => summary.local_actions += count,
        }
    }
    if let Some(stats) = &action_summary.action_cache_statistics {
        summary.action_cache_hits = stats.hits.max(0) as u64;
    }
}

/// The human-readable message of a failure detail, or its debug form.
fn failure_message(detail: &crate::proto::build::bazel::failure_details::FailureDetail) -> String {
    if detail.message.is_empty() {
        format!("{:?}", detail)
    } else {
        detail.message.clone()
    }
}

/// Where a BEP `File` lives: its URI plus the CAS digest and length, which
/// the UI turns into an `/api/blobs` link.
///
/// Bazel leaves the digest and length empty on action stdout and stderr and
/// encodes them only in the bytestream URI (`.../blobs/<hash>/<size>`); they
/// are recovered from there.
fn file_reference(file: &crate::proto::build::bazel::build_event_stream::File) -> Value {
    use crate::proto::build::bazel::build_event_stream::file::File as Location;
    let uri = match &file.file {
        Some(Location::Uri(uri)) => Some(uri.clone()),
        _ => None,
    };
    let (digest, length) = if file.digest.is_empty() {
        uri.as_deref()
            .and_then(blob_from_bytestream_uri)
            .unwrap_or_default()
    } else {
        (file.digest.clone(), file.length)
    };
    serde_json::json!({
        "name": file.name,
        "uri": uri,
        "digest": digest,
        "length": length,
    })
}

/// A `BuildToolLogs` entry: [`file_reference`] plus its inline contents when
/// they are short text, like Bazel's "critical path" summary.
fn build_tool_log_json(file: &crate::proto::build::bazel::build_event_stream::File) -> Value {
    use crate::proto::build::bazel::build_event_stream::file::File as Location;
    let mut log = file_reference(file);
    if let Some(Location::Contents(bytes)) = &file.file {
        if bytes.len() <= MAX_INLINE_TOOL_LOG {
            if let Ok(text) = std::str::from_utf8(bytes) {
                log["contents"] = Value::String(text.to_string());
            }
        }
    }
    log
}

/// The duration in Bazel's "critical path" tool log, whose first line reads
/// `Critical Path: 9.80s, Remote (...)`.
fn critical_path_of(logs: &BuildToolLogs) -> Option<Duration> {
    use crate::proto::build::bazel::build_event_stream::file::File as Location;
    let file = logs.log.iter().find(|f| f.name == "critical path")?;
    let Some(Location::Contents(bytes)) = &file.file else {
        return None;
    };
    let text = std::str::from_utf8(bytes).ok()?;
    let seconds = text
        .strip_prefix("Critical Path: ")?
        .split(['s', ','])
        .next()?
        .parse::<f64>()
        .ok()?;
    Duration::try_from_secs_f64(seconds).ok()
}

fn command_label(summary: &BuildSummary) -> &str {
    if summary.command.is_empty() {
        "unknown"
    } else {
        &summary.command
    }
}

/// The metric label of a build's status, or `None` while it runs.
fn final_status_label(status: BuildStatus) -> Option<&'static str> {
    match status {
        BuildStatus::InProgress => None,
        BuildStatus::Success => Some("success"),
        BuildStatus::Failure => Some("failure"),
        BuildStatus::Interrupted => Some("interrupted"),
    }
}

/// The SHA-256 hash and size encoded in a bytestream URI.
fn blob_from_bytestream_uri(uri: &str) -> Option<(String, i64)> {
    let rest = uri.split("/blobs/").nth(1)?;
    let mut parts = rest.split('/');
    let hash = parts.next()?;
    let size = parts.next()?.parse().ok()?;
    (hash.len() == 64).then(|| (hash.to_string(), size))
}

/// A protobuf timestamp in UTC; the current time when out of range.
fn prost_timestamp_to_datetime(ts: &prost_types::Timestamp) -> DateTime<Utc> {
    DateTime::from_timestamp(ts.seconds, ts.nanos as u32).unwrap_or_else(Utc::now)
}

/// [`prost_timestamp_to_datetime`] formatted as RFC 3339.
fn prost_timestamp_to_rfc3339(ts: &prost_types::Timestamp) -> String {
    prost_timestamp_to_datetime(ts).to_rfc3339()
}

/// The stored and served JSON form of a build event.
///
/// Carries the event id as a readable string, the payload kind and
/// timestamp, and the payload fields the store and the UI read. Payloads
/// with nothing to read are an empty object under their kind.
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
                    "stdout": a.stdout.as_ref().map(file_reference),
                    "stderr": a.stderr.as_ref().map(file_reference),
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
                    "start_time": test_attempt_start(tr),
                    "duration_ms": test_attempt_duration_ms(tr),
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
                    "metadata": m.metadata.iter()
                        .map(|(k, v)| (k.clone(), repair_mojibake(v)))
                        .collect::<HashMap<_, _>>(),
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
                }
            }),
            Payload::NamedSetOfFiles(_) => serde_json::json!({"NamedSetOfFiles": {}}),
            Payload::UnstructuredCommandLine(u) => serde_json::json!({
                "UnstructuredCommandLine": {
                    "args": repair_all(&u.args),
                }
            }),
            Payload::OptionsParsed(o) => serde_json::json!({
                "OptionsParsed": {
                    "cmd_line": repair_all(&o.cmd_line),
                    "explicit_cmd_line": repair_all(&o.explicit_cmd_line),
                    "startup_options": repair_all(&o.startup_options),
                    "explicit_startup_options": repair_all(&o.explicit_startup_options),
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
            Payload::StructuredCommandLine(cl) => serde_json::json!({
                "StructuredCommandLine": {
                    "command_line_label": cl.command_line_label,
                    "args": structured_args(cl),
                }
            }),
            Payload::BuildToolLogs(logs) => serde_json::json!({
                "BuildToolLogs": {
                    "log": logs.log.iter().map(build_tool_log_json).collect::<Vec<_>>(),
                }
            }),
            Payload::TargetSummary(_) => serde_json::json!({"TargetSummary": {}}),
            Payload::ConvenienceSymlinksIdentified(_) => {
                serde_json::json!({"ConvenienceSymlinksIdentified": {}})
            }
            Payload::ExecRequest(_) => serde_json::json!({"ExecRequest": {}}),
            Payload::TestProgress(_) => serde_json::json!({"TestProgress": {}}),
            Payload::Fetch(f) => serde_json::json!({"Fetch": {"success": f.success}}),
        };
    }

    super::redact::event(&mut obj);
    obj
}

/// The `kind` of a stored event: the BEP message name of its payload.
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
        Payload::Fetch(_) => "Fetch".to_string(),
        Payload::StructuredCommandLine(_) => "StructuredCommandLine".to_string(),
    }
}

/// The time a payload refers to, for the payloads that carry one.
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

/// A readable form of an event id: its kind, followed by `:<label>` or
/// `:<patterns>` for ids that carry one.
fn build_event_id_kind(id: &BuildEventId) -> String {
    if let Some(inner) = &id.id {
        match inner {
            build_event_id::Id::Unknown(_) => "Unknown".to_string(),
            build_event_id::Id::Progress(_) => "Progress".to_string(),
            build_event_id::Id::Started(_) => "BuildStarted".to_string(),
            build_event_id::Id::BuildFinished(_) => "BuildFinished".to_string(),
            build_event_id::Id::ActionCompleted(a) => format!("ActionCompleted:{}", a.label),
            build_event_id::Id::TargetCompleted(t) if !t.aspect.is_empty() => {
                format!("AspectCompleted:{}:{}", t.label, t.aspect)
            }
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
            build_event_id::Id::ConfiguredLabel(_) => "ConfiguredLabel".to_string(),
            build_event_id::Id::UnconfiguredLabel(_) => "UnconfiguredLabel".to_string(),
            other => format!("{:?}", other),
        }
    } else {
        "Unknown".to_string()
    }
}

/// The target label of a target or test event id. `None` for the
/// completion of an aspect applied to a target, which is not a run of it.
fn target_label_from_id(id: &BuildEventId) -> Option<String> {
    id.id.as_ref().and_then(|inner| match inner {
        build_event_id::Id::TargetCompleted(t) if !t.aspect.is_empty() => None,
        build_event_id::Id::TargetCompleted(t) => Some(t.label.clone()),
        build_event_id::Id::TargetConfigured(t) => Some(t.label.clone()),
        build_event_id::Id::TestResult(t) => Some(t.label.clone()),
        build_event_id::Id::TestSummary(t) => Some(t.label.clone()),
        _ => None,
    })
}

/// The execution record of a requested target from its `TargetComplete`
/// payload, the build's summary and its action totals.
fn build_target_execution(
    label: &str,
    target_kind: &str,
    invocation_id: &str,
    completed: &Value,
    summary: &BuildSummary,
    metrics: &TargetActionMetrics,
    env_vars: &HashMap<String, String>,
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
        cache_hits: 0,
        cache_misses: 0,
        missed_mnemonics: BTreeMap::new(),
        heaviest_inputs: None,
    }
}

/// The execution record of a test from its stored `TestResult` payload.
fn build_test_execution_json(label: &str, invocation_id: &str, result: &Value) -> TestExecution {
    let status = test_status_name(result.get("status"));
    let start_time = result
        .get("start_time")
        .and_then(|v| v.as_str())
        .map(String::from);
    let duration_ms = result
        .get("duration_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let end_time = parse_rfc3339(start_time.as_deref())
        .map(|start| (start + chrono::Duration::milliseconds(duration_ms as i64)).to_rfc3339());
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
        start_time,
        end_time,
        duration_ms,
    }
}

/// The `TestStatus` name (`PASSED`, `FAILED`...) of a stored status, which
/// the event log holds as the enum's number; a name is taken as is.
fn test_status_name(status: Option<&Value>) -> String {
    match status {
        Some(Value::String(name)) => name.clone(),
        Some(value) => value
            .as_i64()
            .and_then(|n| i32::try_from(n).ok())
            .and_then(|n| TestStatus::try_from(n).ok())
            .unwrap_or(TestStatus::NoStatus)
            .as_str_name()
            .to_string(),
        None => TestStatus::NoStatus.as_str_name().to_string(),
    }
}

/// When a test attempt started, from the timestamp or the deprecated
/// milliseconds Bazel versions before it set.
fn test_attempt_start(tr: &TestResult) -> Option<String> {
    if let Some(ts) = &tr.test_attempt_start {
        return Some(prost_timestamp_to_rfc3339(ts));
    }
    #[allow(deprecated)]
    let millis = tr.test_attempt_start_millis_epoch;
    (millis > 0)
        .then(|| DateTime::from_timestamp_millis(millis))
        .flatten()
        .map(|dt| dt.to_rfc3339())
}

/// How long a test attempt ran, in milliseconds.
fn test_attempt_duration_ms(tr: &TestResult) -> u64 {
    if let Some(d) = &tr.test_attempt_duration {
        return (d.seconds.max(0) as u64) * 1000 + (d.nanos.max(0) as u64) / 1_000_000;
    }
    #[allow(deprecated)]
    let millis = tr.test_attempt_duration_millis;
    millis.max(0) as u64
}

/// The target label of a stored target or test event id, the inverse of
/// [`build_event_id_kind`] for those ids.
fn target_label_from_string_id(id: &str) -> Option<String> {
    [
        "TargetCompleted:",
        "TargetConfigured:",
        "TestResult:",
        "TestSummary:",
    ]
    .iter()
    .find_map(|prefix| id.strip_prefix(prefix))
    .map(str::to_string)
}

/// The environment a build passed to its actions, tests and repository
/// rules: every `--action_env`, `--test_env` and `--repo_env` of its command
/// line, with an inherited variable (`--action_env=NAME`) resolved from the
/// client environment Bazel reports as `--client_env`.
fn extract_env_vars(events: &[Value]) -> HashMap<String, String> {
    let mut client_env = HashMap::new();
    let mut explicit_env = HashMap::new();

    fn extract_from_arg<'a>(s: &'a str, prefix: &str) -> Option<(&'a str, Option<&'a str>)> {
        let rest = s.rsplit_once(prefix).map(|(_, r)| r)?;
        Some(match rest.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (rest, None),
        })
    }

    let mut reader = InputsReader::default();
    for event in events {
        reader.absorb(event);
    }
    let args = reader.finish().command_line;
    for s in &args {
        if let Some((key, Some(value))) = extract_from_arg(s, "--client_env=") {
            client_env.insert(key.to_string(), value.to_string());
        }
    }
    for s in &args {
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
    explicit_env
}

/// Folds one executed action into its target's action totals. An action
/// counts as cached when its `SpawnExec` strategy detail reports a cache hit.
fn update_action_metrics(
    metrics: &mut HashMap<String, TargetActionMetrics>,
    action: &ActionExecuted,
    build_start: Option<DateTime<Utc>>,
) {
    let Some(label) = target_label_from_action_id(action) else {
        return;
    };

    let success = action.success;
    let start = action.start_time.as_ref().map(prost_timestamp_to_datetime);
    let end = action.end_time.as_ref().map(prost_timestamp_to_datetime);
    let duration_ms = action_duration_ms(start, end, build_start);

    let is_cached = action
        .strategy_details
        .iter()
        .filter(|any| any.type_url.ends_with("SpawnExec"))
        .find_map(|any| SpawnExec::decode(&*any.value).ok())
        .map(|spawn| spawn.cache_hit)
        .unwrap_or(false);

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

/// The label of the target an action belongs to, from the deprecated
/// `ActionExecuted.label`, which Bazel still populates; `None` when empty.
fn target_label_from_action_id(action: &ActionExecuted) -> Option<String> {
    (!action.label.is_empty()).then(|| action.label.clone())
}

/// The structured JSON form of an event id, as listed in `children`.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_critical_path_is_read_from_bazels_tool_log() {
        use crate::proto::build::bazel::build_event_stream::{file::File as Location, File};
        let log = |name: &str, text: &str| File {
            name: name.to_string(),
            file: Some(Location::Contents(text.as_bytes().to_vec())),
            ..Default::default()
        };
        let logs = BuildToolLogs {
            log: vec![
                log("elapsed time", "19.250000"),
                log(
                    "critical path",
                    "Critical Path: 9.80s, Remote (99.94% of the time): [queue: 81.17%]\n  0.00s runfiles",
                ),
            ],
        };
        assert_eq!(critical_path_of(&logs), Some(Duration::from_millis(9800)));
        assert_eq!(critical_path_of(&BuildToolLogs::default()), None);
    }
    use crate::proto::build::bazel::build_event_stream::build_metrics::action_summary::RunnerCount;
    use crate::proto::build::bazel::build_event_stream::build_metrics::ActionSummary;
    use prost::Message;

    fn runner(name: &str, count: i32) -> RunnerCount {
        RunnerCount {
            name: name.to_string(),
            count,
            exec_kind: String::new(),
        }
    }

    #[test]
    fn decodes_build_metrics_emitted_by_bazel_8_7_0() {
        let bytes = include_bytes!("testdata/bazel_8_7_0_build_metrics.bin");
        let event = BuildEvent::decode(bytes.as_slice()).expect("BuildMetrics event must decode");
        let Some(Payload::BuildMetrics(metrics)) = event.payload else {
            panic!("fixture is not a BuildMetrics event");
        };
        let action_summary = metrics.action_summary.expect("action_summary present");
        assert_eq!(action_summary.actions_executed, 1);
        assert!(action_summary.action_cache_statistics.is_some());
        assert!(action_summary
            .runner_count
            .iter()
            .any(|r| r.name == "internal" && r.count == 1));
    }

    #[tokio::test]
    async fn running_builds_count_actions_from_the_action_cache() {
        let temp = tempfile::TempDir::new().unwrap();
        let bus = LiveBus::new();
        let log = Arc::new(ActionCacheLog::new());
        let storage = storage_in(temp.path())
            .await
            .with_live_bus(bus.clone())
            .with_action_cache_log(log.clone());
        storage
            .store_event("live", started("live", "2026-10-02T10:00:00Z"))
            .await
            .unwrap();
        for hit in [true, true, true, false] {
            log.record("live", "//a:lib", "CppCompile", hit).await;
        }
        let mut updates = bus.subscribe();

        storage.refresh_live_counters().await;
        let build = storage.get_build("live").await.unwrap().unwrap();
        assert_eq!(
            (
                build.total_actions,
                build.cached_actions,
                build.local_actions
            ),
            (4, 3, 1)
        );
        match updates.try_recv().unwrap() {
            LiveEvent::BuildUpdated { build } => assert_eq!(build.total_actions, 4),
            other => panic!("expected BuildUpdated, got {other:?}"),
        }
        storage.refresh_live_counters().await;
        assert!(
            updates.try_recv().is_err(),
            "unchanged counts are not republished"
        );

        storage
            .store_event(
                "live",
                BuildEvent {
                    payload: Some(Payload::BuildMetrics(BuildMetrics {
                        action_summary: Some(ActionSummary {
                            runner_count: vec![runner("total", 10), runner("remote cache hit", 7)],
                            ..Default::default()
                        }),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        log.record("live", "//a:lib", "CppCompile", false).await;
        storage.refresh_live_counters().await;
        let build = storage.get_build("live").await.unwrap().unwrap();
        assert_eq!((build.total_actions, build.cached_actions), (10, 7));
    }

    #[tokio::test]
    async fn a_target_runs_once_per_build_whatever_its_configurations_and_aspects() {
        use crate::proto::build::bazel::build_event_stream::build_event_id::TargetCompletedId;
        let completed = |config: &str, aspect: &str, success: bool| {
            BuildEvent {
            id: Some(BuildEventId {
                id: Some(build_event_id::Id::TargetCompleted(TargetCompletedId {
                    label: "//a:lib".to_string(),
                    configuration: Some(
                        crate::proto::build::bazel::build_event_stream::build_event_id::ConfigurationId {
                            id: config.to_string(),
                        },
                    ),
                    aspect: aspect.to_string(),
                })),
            }),
            payload: Some(Payload::Completed(TargetComplete {
                success,
                ..Default::default()
            })),
            ..Default::default()
        }
        };
        let temp = tempfile::TempDir::new().unwrap();
        let storage = storage_in(temp.path()).await;
        storage
            .store_event("multi", started("multi", "2026-10-02T10:00:00Z"))
            .await
            .unwrap();
        for event in [
            completed("arm64", "", true),
            completed("amd64", "", false),
            completed("arm64", "//tools/lint:ruff.bzl%ruff", true),
            completed("arm64", "//tools/lint:pruner.bzl%pruner", true),
        ] {
            storage.store_event("multi", event).await.unwrap();
        }
        storage
            .store_event("multi", finished(false, "2026-10-02T10:01:00Z"))
            .await
            .unwrap();
        storage.store_event("multi", last_message()).await.unwrap();

        let targets = storage.get_build_targets("multi").await.unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].status, TargetExecutionStatus::Failure);
        assert_eq!(
            storage.get_target_history("//a:lib").await.unwrap().len(),
            1
        );
    }

    #[test]
    fn runner_counts_split_cache_hits_from_executed_spawns() {
        let mut summary = BuildSummary::default();
        let metrics = BuildMetrics {
            action_summary: Some(ActionSummary {
                runner_count: vec![
                    runner("total", 66507),
                    runner("remote cache hit", 34809),
                    runner("internal", 31399),
                    runner("linux-sandbox", 298),
                    runner("local", 1),
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        handle_build_metrics(&mut summary, &metrics);
        assert_eq!(summary.total_actions, 66507);
        assert_eq!(summary.remote_cache_hits, 34809);
        assert_eq!(summary.cached_actions, 34809);
        assert_eq!(summary.local_actions, 299);
    }

    #[test]
    fn the_runner_split_keeps_rbe_workers_apart_from_the_bazel_host() {
        let mut summary = BuildSummary::default();
        let metrics = BuildMetrics {
            action_summary: Some(ActionSummary {
                runner_count: vec![
                    runner("total", 77236),
                    runner("internal", 45470),
                    runner("linux-sandbox", 5),
                    runner("local", 17),
                    runner("remote", 31744),
                    runner("worker", 0),
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        handle_build_metrics(&mut summary, &metrics);
        assert_eq!(
            summary.runners,
            BTreeMap::from([
                ("internal".to_string(), 45470),
                ("linux-sandbox".to_string(), 5),
                ("local".to_string(), 17),
                ("remote".to_string(), 31744),
            ])
        );
        assert_eq!(summary.total_actions, 77236);
        assert_eq!(summary.local_actions, 5 + 17 + 31744);
    }

    #[test]
    fn pattern_and_metadata_land_in_the_summary() {
        use crate::proto::build::bazel::build_event_stream::{
            build_event_id::PatternExpandedId, BuildMetadata as ProtoBuildMetadata,
        };
        let mut summary = BuildSummary::default();
        let mut known = HashSet::new();
        let pattern = BuildEvent {
            id: Some(BuildEventId {
                id: Some(build_event_id::Id::Pattern(PatternExpandedId {
                    pattern: vec!["//...".to_string()],
                })),
            }),
            ..Default::default()
        };
        let metadata = BuildEvent {
            payload: Some(Payload::BuildMetadata(ProtoBuildMetadata {
                metadata: [("USER".to_string(), "alice".to_string())]
                    .into_iter()
                    .collect(),
            })),
            ..Default::default()
        };
        update_summary(&mut summary, &mut known, &pattern);
        update_summary(&mut summary, &mut known, &metadata);
        assert_eq!(summary.pattern, vec!["//...".to_string()]);
        assert_eq!(
            summary.metadata.get("USER").map(String::as_str),
            Some("alice")
        );
    }

    #[test]
    fn bytestream_uris_yield_the_blob_digest() {
        assert_eq!(
            blob_from_bytestream_uri(&format!(
                "bytestream://rbe-cache:9094/blobs/{}/30",
                "4c".repeat(32)
            )),
            Some(("4c".repeat(32), 30))
        );
        assert_eq!(blob_from_bytestream_uri("file:///tmp/stderr"), None);
    }

    #[test]
    fn double_encoded_metadata_is_repaired_and_clean_text_kept() {
        assert_eq!(repair_mojibake("JosÃ© PÃ©rez"), "José Pérez");
        assert_eq!(repair_mojibake("José Pérez"), "José Pérez");
        assert_eq!(repair_mojibake("alice"), "alice");
        assert_eq!(
            repair_mojibake("fix: añadir métricas"),
            "fix: añadir métricas"
        );
    }

    #[test]
    fn action_events_only_count_totals_and_failures() {
        let mut summary = BuildSummary::default();
        let mut known = HashSet::new();
        let ok = ActionExecuted {
            success: true,
            ..Default::default()
        };
        let failed = ActionExecuted {
            success: false,
            ..Default::default()
        };
        handle_action(&mut summary, &mut known, None, &ok);
        handle_action(&mut summary, &mut known, None, &failed);
        assert_eq!(summary.total_actions, 2);
        assert_eq!(summary.failed_actions, 1);
        assert_eq!(summary.cached_actions, 0);
        assert_eq!(summary.local_actions, 0);
    }

    fn execution_for(label: &str, invocation_id: &str) -> TargetExecution {
        TargetExecution {
            label: label.to_string(),
            target_kind: "rust_library".to_string(),
            invocation_id: invocation_id.to_string(),
            status: TargetExecutionStatus::Success,
            tags: vec![],
            build_start_time: None,
            build_end_time: None,
            build_duration_ms: 1,
            action_duration_ms: 1,
            action_count: 1,
            cached_actions: 0,
            failed_actions: 0,
            env_vars: HashMap::new(),
            cache_hits: 0,
            cache_misses: 0,
            missed_mnemonics: BTreeMap::new(),
            heaviest_inputs: None,
        }
    }

    async fn storage_in(dir: &std::path::Path) -> BesStorage {
        let storage = BesStorage::new(BesConfig {
            data_dir: dir.to_path_buf(),
            max_retention_days: None,
        })
        .await
        .unwrap();
        storage.warm_up().await.unwrap();
        storage
    }

    #[tokio::test]
    async fn warm_up_drops_build_directories_left_empty() {
        let temp = tempfile::TempDir::new().unwrap();
        for dir in ["stale-empty", "stale-hollow", "fresh-empty", "with-events"] {
            std::fs::create_dir(temp.path().join(dir)).unwrap();
        }
        std::fs::write(temp.path().join("with-events/events.jsonl"), "{}\n").unwrap();
        std::fs::write(temp.path().join("stale-hollow/targets.jsonl"), "").unwrap();
        let two_hours_ago = std::time::SystemTime::now() - Duration::from_secs(2 * 3600);
        for dir in ["stale-empty", "stale-hollow", "with-events"] {
            std::fs::File::open(temp.path().join(dir))
                .unwrap()
                .set_modified(two_hours_ago)
                .unwrap();
        }

        let storage = storage_in(temp.path()).await;
        let mut listed: Vec<String> = storage
            .list_builds()
            .await
            .unwrap()
            .into_iter()
            .map(|b| b.invocation_id)
            .collect();
        listed.sort();
        assert_eq!(listed, vec!["fresh-empty", "with-events"]);
        assert!(!temp.path().join("stale-empty").exists());
        assert!(!temp.path().join("stale-hollow").exists());
    }

    fn write_event_log(dir: &std::path::Path, id: &str, lines: &[&str], age: Duration) {
        std::fs::create_dir_all(dir.join(id)).unwrap();
        let path = dir.join(id).join("events.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        std::fs::File::options()
            .append(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - age)
            .unwrap();
    }

    #[tokio::test]
    async fn builds_left_without_a_stream_are_closed_from_their_event_log() {
        let temp = tempfile::TempDir::new().unwrap();
        let started = r#"{"kind":"BuildStarted","payload":{"Started":{"command":"build","workspace_directory":"/ws","start_time":"2026-09-30T10:00:00+00:00"}}}"#;
        let ok = r#"{"kind":"ActionExecuted","payload":{"ActionExecuted":{"success":true}}}"#;
        let failed = r#"{"kind":"ActionExecuted","payload":{"ActionExecuted":{"success":false}}}"#;
        let finished = r#"{"kind":"BuildFinished","payload":{"Finished":{"overall_success":false,"exit_code":1,"finish_time":"2026-09-30T10:05:00+00:00"}}}"#;
        let two_hours = Duration::from_secs(2 * 3600);
        write_event_log(temp.path(), "cut", &[started, ok, failed], two_hours);
        write_event_log(temp.path(), "ended", &[started, ok, finished], two_hours);
        write_event_log(
            temp.path(),
            "quiet-briefly",
            &[started, ok],
            Duration::from_secs(60),
        );

        let storage = storage_in(temp.path()).await;
        storage.recover_orphans().await;

        let cut = storage.get_build("cut").await.unwrap().unwrap();
        assert_eq!(cut.status, BuildStatus::Interrupted);
        assert_eq!(cut.command, "build");
        assert_eq!(cut.start_time.as_deref(), Some("2026-09-30T10:00:00+00:00"));
        assert!(cut.end_time.is_some());
        assert_eq!((cut.total_actions, cut.failed_actions), (2, 1));
        assert!(temp.path().join("cut/summary.json").exists());

        let ended = storage.get_build("ended").await.unwrap().unwrap();
        assert_eq!(ended.status, BuildStatus::Failure);
        assert_eq!(ended.duration_ms, 5 * 60 * 1000);

        let listed: HashMap<String, BuildStatus> = storage
            .list_builds()
            .await
            .unwrap()
            .into_iter()
            .map(|b| (b.invocation_id, b.status))
            .collect();
        assert_eq!(listed["cut"], BuildStatus::Interrupted);
        assert_eq!(listed["quiet-briefly"], BuildStatus::InProgress);
        assert!(!temp.path().join("quiet-briefly/summary.json").exists());
    }

    #[tokio::test]
    async fn a_running_build_serves_its_inputs() {
        use crate::proto::build::bazel::build_event_stream::{
            OptionsParsed, UnstructuredCommandLine,
        };
        let temp = tempfile::TempDir::new().unwrap();
        let storage = storage_in(temp.path()).await;
        storage
            .store_event("live", started("live", "2026-10-02T10:00:00Z"))
            .await
            .unwrap();
        let args = [
            "bazel",
            "--output_base=/ob",
            "build",
            "--client_env=HOME=/home/ci",
            "--client_env=CI_JOB_TOKEN=s3cr3t",
            "--config=ci",
            "//...",
        ];
        storage
            .store_event(
                "live",
                BuildEvent {
                    payload: Some(Payload::UnstructuredCommandLine(UnstructuredCommandLine {
                        args: args.iter().map(|a| a.to_string()).collect(),
                    })),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        storage
            .store_event(
                "live",
                BuildEvent {
                    payload: Some(Payload::OptionsParsed(OptionsParsed {
                        startup_options: vec!["--output_base=/ob".into()],
                        cmd_line: vec!["--config=ci".into(), "--jobs=64".into()],
                        explicit_cmd_line: vec!["--config=ci".into()],
                        ..Default::default()
                    })),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let inputs = storage.get_build_inputs("live").await.unwrap().unwrap();
        assert_eq!(inputs.command_line.len(), args.len());
        assert_eq!(inputs.startup_options, vec!["--output_base=/ob"]);
        assert_eq!(inputs.options, vec!["--config=ci", "--jobs=64"]);
        assert_eq!(inputs.explicit_options, vec!["--config=ci"]);
        assert_eq!(inputs.env["HOME"], "/home/ci");
        assert_eq!(inputs.env["CI_JOB_TOKEN"], super::super::redact::REDACTED);
        assert!(storage.get_build_inputs("unknown").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_stored_build_serves_its_inputs_redacted() {
        let temp = tempfile::TempDir::new().unwrap();
        let lines = [
            r#"{"kind":"BuildStarted","payload":{"Started":{"command":"test"}}}"#,
            r#"{"kind":"UnstructuredCommandLine","payload":{"UnstructuredCommandLine":{"args":["bazel","test","--client_env=DEPLOY_TOKEN=plain","//a:b"]}}}"#,
            r#"{"kind":"OptionsParsed","payload":{"OptionsParsed":{"cmd_line":["--client_env=DEPLOY_TOKEN=plain"],"startup_options":[]}}}"#,
            r#"{"kind":"WorkspaceStatus","payload":{"WorkspaceStatus":{"item":[{"key":"BUILD_USER","value":"ci"},{"key":"BUILD_HOST","value":"runner-1"}]}}}"#,
            r#"{"kind":"Progress","payload":{"Progress":{"stdout":"","stderr":"x"}}}"#,
        ];
        std::fs::create_dir_all(temp.path().join("stored")).unwrap();
        std::fs::write(
            temp.path().join("stored/events.jsonl"),
            lines.join("\n") + "\n",
        )
        .unwrap();
        let storage = storage_in(temp.path()).await;

        let inputs = storage.get_build_inputs("stored").await.unwrap().unwrap();
        assert_eq!(inputs.env["DEPLOY_TOKEN"], super::super::redact::REDACTED);
        assert!(!inputs.command_line.iter().any(|a| a.contains("plain")));
        assert!(!inputs.options.iter().any(|a| a.contains("plain")));
        assert!(inputs.explicit_options.is_empty());
        assert_eq!(inputs.workspace_status["BUILD_USER"], "ci");
        assert_eq!(inputs.workspace_status["BUILD_HOST"], "runner-1");
    }

    #[tokio::test]
    async fn a_run_build_serves_its_original_command_line_redacted_and_repaired() {
        use crate::proto::build::bazel::build_event_stream::{
            BuildMetadata as ProtoBuildMetadata, UnstructuredCommandLine,
        };
        use crate::proto::build::bazel::command_line::{
            command_line_section::SectionType, ChunkList, CommandLine, CommandLineSection,
            Option as CommandLineOption, OptionList,
        };
        let chunks = |chunk: &[&str]| CommandLineSection {
            section_type: Some(SectionType::ChunkList(ChunkList {
                chunk: chunk.iter().map(|c| c.to_string()).collect(),
            })),
            ..Default::default()
        };
        let options = |forms: &[&str]| CommandLineSection {
            section_type: Some(SectionType::OptionList(OptionList {
                option: forms
                    .iter()
                    .map(|f| CommandLineOption {
                        combined_form: f.to_string(),
                        ..Default::default()
                    })
                    .collect(),
            })),
            ..Default::default()
        };
        let command_line = |label: &str, forms: &[&str]| BuildEvent {
            payload: Some(Payload::StructuredCommandLine(CommandLine {
                command_line_label: label.to_string(),
                sections: vec![
                    chunks(&["bazel"]),
                    options(&[""]),
                    chunks(&["run"]),
                    options(forms),
                    chunks(&["//:cli", "REDACTED"]),
                ],
            })),
            ..Default::default()
        };
        let temp = tempfile::TempDir::new().unwrap();
        let storage = storage_in(temp.path()).await;
        let events = [
            started("run", "2026-10-05T10:00:00Z"),
            BuildEvent {
                payload: Some(Payload::UnstructuredCommandLine(UnstructuredCommandLine {
                    args: vec!["REDACTED".to_string()],
                })),
                ..Default::default()
            },
            command_line(
                "original",
                &[
                    "--client_env=CI_REPOSITORY_URL=https://gitlab-ci-token:job-secret@code.example/r.git",
                    "--build_metadata=USER=Jos\u{c3}\u{a9}",
                ],
            ),
            command_line("canonical", &["--canonical"]),
            BuildEvent {
                payload: Some(Payload::BuildMetadata(ProtoBuildMetadata {
                    metadata: HashMap::from([("USER".to_string(), "Jos\u{c3}\u{a9}".to_string())]),
                })),
                ..Default::default()
            },
        ];
        for event in events {
            storage.store_event("run", event).await.unwrap();
        }

        let inputs = storage.get_build_inputs("run").await.unwrap().unwrap();
        assert_eq!(
            inputs.command_line,
            [
                "bazel",
                "run",
                "--client_env=CI_REPOSITORY_URL=https://gitlab-ci-token:<redacted>@code.example/r.git",
                "--build_metadata=USER=José",
                "//:cli",
                "REDACTED",
            ]
        );
        assert_eq!(
            inputs.env["CI_REPOSITORY_URL"],
            "https://gitlab-ci-token:<redacted>@code.example/r.git"
        );
        let stored = storage.get_events("run", false).await.unwrap();
        let serialized = serde_json::to_string(&stored).unwrap();
        assert!(!serialized.contains("job-secret"));
        assert!(serialized.contains(r#""USER":"José""#));
    }

    #[test]
    fn target_aggregates_time_only_the_executions_that_ran_actions() {
        let mut aggregate = TargetAggregate::default();
        for duration in [0, 100, 300] {
            let mut ex = execution_for("//a:b", "i");
            ex.build_duration_ms = 60_000;
            ex.action_duration_ms = duration;
            aggregate.add(&ex);
        }
        let summary = aggregate.summary("//a:b");
        assert_eq!(summary.total_action_ms, 400);
        assert_eq!(summary.max_action_ms, 300);
        assert_eq!(summary.mean_action_ms, 200);
        assert_eq!(
            TargetAggregate::default().summary("//c:d").mean_action_ms,
            0
        );
    }

    #[test]
    fn a_replayed_cached_action_has_no_duration() {
        let at = |t: &str| parse_rfc3339(Some(t));
        let build = at("2026-09-18T21:51:46Z");
        assert_eq!(
            action_duration_ms(
                at("2026-09-12T14:50:50Z"),
                at("2026-09-18T08:02:56Z"),
                build
            ),
            0
        );
        assert_eq!(
            action_duration_ms(
                at("2026-09-18T21:51:50Z"),
                at("2026-09-18T21:51:53Z"),
                build
            ),
            3000
        );
        assert_eq!(
            action_duration_ms(
                at("2026-09-18T21:51:30Z"),
                at("2026-09-18T21:51:31Z"),
                build
            ),
            1000
        );
        assert_eq!(
            action_duration_ms(None, at("2026-09-18T21:51:53Z"), build),
            0
        );
    }

    #[test]
    fn legacy_records_with_replayed_times_are_not_timed() {
        let mut aggregate = TargetAggregate::default();
        let mut replayed = execution_for("//a:b", "old");
        replayed.build_duration_ms = 250_000;
        replayed.action_duration_ms = 493_925_786;
        aggregate.add(&replayed);
        let mut real = execution_for("//a:b", "new");
        real.build_duration_ms = 250_000;
        real.action_duration_ms = 4_000;
        aggregate.add(&real);
        let summary = aggregate.summary("//a:b");
        assert_eq!(
            (
                summary.total_action_ms,
                summary.max_action_ms,
                summary.mean_action_ms
            ),
            (4_000, 4_000, 4_000)
        );
    }

    #[test]
    fn test_aggregates_count_runs_without_a_status() {
        let mut aggregate = TestAggregate::default();
        for status in ["PASSED", "NO_STATUS", "", "FAILED"] {
            aggregate.add(&TestExecution {
                label: "//a:t".to_string(),
                invocation_id: "i".to_string(),
                status: status.to_string(),
                cached_locally: false,
                cached_remotely: false,
                start_time: None,
                end_time: None,
                duration_ms: 0,
            });
        }
        let summary = aggregate.summary("//a:t");
        assert_eq!(
            (
                summary.passed_count,
                summary.failed_count,
                summary.no_status_count
            ),
            (1, 1, 2)
        );
    }

    #[tokio::test]
    async fn test_results_keep_their_status_start_and_duration() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = storage_in(temp.path()).await;
        storage
            .store_event("tests", started("tests", "2026-10-02T10:00:00Z"))
            .await
            .unwrap();
        for (label, status) in [
            ("//a:ok_test", TestStatus::Passed),
            ("//a:bad_test", TestStatus::Failed),
        ] {
            storage
                .store_event(
                    "tests",
                    BuildEvent {
                        id: Some(BuildEventId {
                            id: Some(build_event_id::Id::TestResult(
                                crate::proto::build::bazel::build_event_stream::build_event_id::TestResultId {
                                    label: label.to_string(),
                                    ..Default::default()
                                },
                            )),
                        }),
                        payload: Some(Payload::TestResult(TestResult {
                            status: status as i32,
                            test_attempt_start: Some(prost_types::Timestamp {
                                seconds: DateTime::parse_from_rfc3339("2026-10-02T10:00:05Z")
                                    .unwrap()
                                    .timestamp(),
                                nanos: 0,
                            }),
                            test_attempt_duration: Some(prost_types::Duration {
                                seconds: 2,
                                nanos: 500_000_000,
                            }),
                            ..Default::default()
                        })),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }
        storage
            .store_event("tests", finished(false, "2026-10-02T10:01:00Z"))
            .await
            .unwrap();
        storage.store_event("tests", last_message()).await.unwrap();

        let ok = storage.get_test_history("//a:ok_test").await.unwrap();
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].status, "PASSED");
        assert_eq!(ok[0].duration_ms, 2500);
        assert_eq!(
            parse_rfc3339(ok[0].start_time.as_deref()),
            parse_rfc3339(Some("2026-10-02T10:00:05Z"))
        );
        assert_eq!(
            parse_rfc3339(ok[0].end_time.as_deref()),
            parse_rfc3339(Some("2026-10-02T10:00:07.500Z"))
        );
        let bad = storage.get_test_history("//a:bad_test").await.unwrap();
        assert_eq!(bad[0].status, "FAILED");

        let tests = storage.list_tests().await.unwrap();
        let passed: usize = tests.iter().map(|t| t.passed_count).sum();
        let failed: usize = tests.iter().map(|t| t.failed_count).sum();
        assert_eq!((passed, failed), (1, 1));
    }

    #[test]
    fn stored_test_statuses_read_as_names() {
        assert_eq!(test_status_name(Some(&serde_json::json!(1))), "PASSED");
        assert_eq!(test_status_name(Some(&serde_json::json!(4))), "FAILED");
        assert_eq!(test_status_name(Some(&serde_json::json!(99))), "NO_STATUS");
        assert_eq!(test_status_name(Some(&serde_json::json!("FLAKY"))), "FLAKY");
        assert_eq!(test_status_name(None), "NO_STATUS");
    }

    #[tokio::test]
    async fn warm_up_keeps_builds_that_streamed_in_before_it() {
        let temp = tempfile::TempDir::new().unwrap();
        let earlier = storage_in(temp.path()).await;
        record_build(
            &earlier,
            "on-disk",
            "2030-01-01T10:00:00Z",
            "2030-01-01T10:01:00Z",
            true,
        )
        .await;
        drop(earlier);

        let storage = BesStorage::new(BesConfig {
            data_dir: temp.path().to_path_buf(),
            max_retention_days: None,
        })
        .await
        .unwrap();
        storage
            .store_event("live", started("live", "2030-01-02T10:00:00Z"))
            .await
            .unwrap();
        assert_eq!(
            storage.list_builds().await.unwrap().len(),
            1,
            "before warm_up the listing holds only the live build"
        );

        storage.warm_up().await.unwrap();
        let builds = storage.list_builds().await.unwrap();
        let ids: Vec<&str> = builds.iter().map(|b| b.invocation_id.as_str()).collect();
        assert_eq!(ids, vec!["live", "on-disk"]);
        assert_eq!(
            storage.get_build("live").await.unwrap().unwrap().status,
            BuildStatus::InProgress
        );
    }

    fn started(invocation_id: &str, start: &str) -> BuildEvent {
        BuildEvent {
            id: Some(BuildEventId {
                id: Some(build_event_id::Id::Started(
                    crate::proto::build::bazel::build_event_stream::build_event_id::BuildStartedId {},
                )),
            }),
            payload: Some(Payload::Started(BuildStarted {
                uuid: invocation_id.to_string(),
                command: "build".to_string(),
                start_time: Some(prost_types::Timestamp {
                    seconds: DateTime::parse_from_rfc3339(start).unwrap().timestamp(),
                    nanos: 0,
                }),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    fn finished(success: bool, end: &str) -> BuildEvent {
        BuildEvent {
            payload: Some(Payload::Finished(BuildFinished {
                overall_success: success,
                finish_time: Some(prost_types::Timestamp {
                    seconds: DateTime::parse_from_rfc3339(end).unwrap().timestamp(),
                    nanos: 0,
                }),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    fn last_message() -> BuildEvent {
        BuildEvent {
            last_message: true,
            ..Default::default()
        }
    }

    async fn record_build(storage: &BesStorage, id: &str, start: &str, end: &str, ok: bool) {
        storage.store_event(id, started(id, start)).await.unwrap();
        storage.store_event(id, finished(ok, end)).await.unwrap();
        storage.store_event(id, last_message()).await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_target_appends_stay_line_delimited() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(storage_in(temp.path()).await);
        let tasks = (0..60).map(|i| {
            let storage = storage.clone();
            tokio::spawn(async move {
                storage
                    .append_target_execution(&execution_for(
                        &format!("//t:{i}"),
                        &format!("inv-{i}"),
                    ))
                    .await
                    .unwrap();
            })
        });
        for task in tasks {
            task.await.unwrap();
        }
        let content = std::fs::read_to_string(temp.path().join("targets.jsonl")).unwrap();
        let mut count = 0;
        for line in content.lines() {
            serde_json::from_str::<TargetExecution>(line)
                .unwrap_or_else(|e| panic!("line {count} does not parse: {e}: {line}"));
            count += 1;
        }
        assert_eq!(count, 60);
    }

    #[tokio::test]
    async fn load_salvages_records_concatenated_on_one_line() {
        let temp = tempfile::TempDir::new().unwrap();
        let first = serde_json::to_string(&execution_for("//a:first", "inv-1")).unwrap();
        let second = serde_json::to_string(&execution_for("//b:second", "inv-2")).unwrap();
        let third = serde_json::to_string(&execution_for("//c:third", "inv-3")).unwrap();
        std::fs::write(
            temp.path().join("targets.jsonl"),
            format!("{first}{second}\n{third}\n"),
        )
        .unwrap();

        let storage = storage_in(temp.path()).await;
        let mut labels: Vec<String> = storage
            .list_targets()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.label)
            .collect();
        labels.sort();
        assert_eq!(labels, vec!["//a:first", "//b:second", "//c:third"]);

        storage.wait_label_index().await;
        for (label, invocation_id) in [("//a:first", "inv-1"), ("//b:second", "inv-2")] {
            let history = storage.get_target_history(label).await.unwrap();
            assert_eq!(
                history.len(),
                1,
                "{label} must reach its per-label history even when its record shared a line"
            );
            assert_eq!(history[0].invocation_id, invocation_id);
        }
    }

    #[tokio::test]
    async fn finished_builds_leave_memory_and_stay_listed() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = storage_in(temp.path()).await;
        record_build(
            &storage,
            "inv-a",
            "2026-09-01T10:00:00Z",
            "2026-09-01T10:01:00Z",
            true,
        )
        .await;

        assert!(storage.invocations.is_empty());
        let listed = storage.list_builds().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, BuildStatus::Success);
        assert!(temp.path().join("inv-a/summary.json").exists());
        assert!(temp.path().join("inv-a/targets.jsonl").exists());

        let events = storage.get_events("inv-a", false).await.unwrap();
        assert_eq!(events.len(), 3);
        let build = storage.get_build("inv-a").await.unwrap().unwrap();
        assert_eq!(build.status, BuildStatus::Success);
        assert!(storage.get_build("missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn listing_pages_newest_first_with_cursor() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = storage_in(temp.path()).await;
        for i in 1..=5 {
            let start = format!("2026-09-0{i}T10:00:00Z");
            let end = format!("2026-09-0{i}T10:05:00Z");
            record_build(&storage, &format!("inv-{i}"), &start, &end, i % 2 == 0).await;
        }
        let first = storage.list_builds_page(2, None, |_| true).await.unwrap();
        assert_eq!(first.total, 5);
        let ids: Vec<&str> = first
            .builds
            .iter()
            .map(|b| b.invocation_id.as_str())
            .collect();
        assert_eq!(ids, vec!["inv-5", "inv-4"]);
        let cursor = first.next_cursor.expect("more pages");

        let second = storage
            .list_builds_page(2, Some(&cursor), |_| true)
            .await
            .unwrap();
        let ids: Vec<&str> = second
            .builds
            .iter()
            .map(|b| b.invocation_id.as_str())
            .collect();
        assert_eq!(ids, vec!["inv-3", "inv-2"]);
        let cursor = second.next_cursor.expect("last page pending");

        let third = storage
            .list_builds_page(2, Some(&cursor), |_| true)
            .await
            .unwrap();
        let ids: Vec<&str> = third
            .builds
            .iter()
            .map(|b| b.invocation_id.as_str())
            .collect();
        assert_eq!(ids, vec!["inv-1"]);
        assert!(third.next_cursor.is_none());

        let failed = storage
            .list_builds_page(10, None, |b| b.status == BuildStatus::Failure)
            .await
            .unwrap();
        assert_eq!(failed.total, 3);

        let stats = storage.stats().await.unwrap();
        assert_eq!(stats.total_builds, 5);
        assert_eq!(stats.successful_builds, 2);
        assert_eq!(stats.failed_builds, 3);
    }

    #[tokio::test]
    async fn storing_events_records_the_lock_wait_and_the_event_time() {
        let temp = tempfile::TempDir::new().unwrap();
        let metrics = Arc::new(Metrics::new());
        let storage = storage_in(temp.path()).await.with_metrics(metrics.clone());
        record_build(
            &storage,
            "inv-m",
            "2026-09-01T10:00:00Z",
            "2026-09-01T10:01:00Z",
            true,
        )
        .await;

        let output = metrics.gather();
        for phase in ["lock_wait", "event"] {
            let count = output
                .lines()
                .find(|l| {
                    l.starts_with(&format!("rbe_bes_store_seconds_count{{phase=\"{phase}\"}}"))
                })
                .unwrap_or_else(|| panic!("no {phase} series"));
            assert_ne!(count.rsplit(' ').next(), Some("0"), "{count}");
        }
    }

    #[tokio::test]
    async fn index_survives_a_restart() {
        let temp = tempfile::TempDir::new().unwrap();
        {
            let storage = storage_in(temp.path()).await;
            record_build(
                &storage,
                "inv-a",
                "2026-09-01T10:00:00Z",
                "2026-09-01T10:01:00Z",
                true,
            )
            .await;
        }
        let storage = storage_in(temp.path()).await;
        let listed = storage.list_builds().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].invocation_id, "inv-a");
    }

    #[tokio::test]
    async fn retention_drops_old_builds_and_compacts_histories() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut storage = storage_in(temp.path()).await;
        record_build(
            &storage,
            "old",
            "2020-01-01T10:00:00Z",
            "2020-01-01T10:01:00Z",
            true,
        )
        .await;
        record_build(
            &storage,
            "new",
            "2099-01-01T10:00:00Z",
            "2099-01-01T10:01:00Z",
            true,
        )
        .await;
        storage
            .append_target_execution(&execution_for("//a:lib", "old"))
            .await
            .unwrap();
        storage
            .append_target_execution(&execution_for("//a:lib", "new"))
            .await
            .unwrap();

        storage.config.max_retention_days = Some(30);
        let removed = storage.apply_retention().await.unwrap();
        assert_eq!(removed, 1);
        assert!(!temp.path().join("old").exists());
        assert!(temp.path().join("new").exists());
        assert_eq!(storage.list_builds().await.unwrap().len(), 1);

        let history = storage.get_target_history("//a:lib").await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].invocation_id, "new");
        let targets = storage.list_targets().await.unwrap();
        assert_eq!(targets[0].total_executions, 1);
    }

    #[tokio::test]
    async fn build_targets_come_from_the_invocation_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = storage_in(temp.path()).await;
        record_build(
            &storage,
            "inv-a",
            "2026-09-01T10:00:00Z",
            "2026-09-01T10:01:00Z",
            true,
        )
        .await;
        std::fs::create_dir_all(temp.path().join("legacy")).unwrap();
        storage
            .append_target_execution(&execution_for("//x:legacy", "legacy"))
            .await
            .unwrap();

        assert!(storage.get_build_targets("inv-a").await.unwrap().is_empty());
        let legacy = storage.get_build_targets("legacy").await.unwrap();
        assert_eq!(legacy.len(), 1);
        assert_eq!(legacy[0].label, "//x:legacy");
        assert!(temp.path().join("legacy/targets.jsonl").exists());
        std::fs::remove_file(temp.path().join("targets.jsonl")).unwrap();
        let again = storage.get_build_targets("legacy").await.unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].label, "//x:legacy");
        assert!(storage
            .get_build_targets("unknown")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn a_running_build_does_not_read_the_global_history() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = storage_in(temp.path()).await;
        storage
            .store_event("running", started("running", "2026-09-01T10:00:00Z"))
            .await
            .unwrap();
        storage
            .append_target_execution(&execution_for("//x:stale", "running"))
            .await
            .unwrap();

        assert!(storage
            .get_build_targets("running")
            .await
            .unwrap()
            .is_empty());
        assert!(!temp.path().join("running/targets.jsonl").exists());
    }

    #[tokio::test]
    async fn reading_the_targets_of_a_build_without_events_writes_nothing() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = storage_in(temp.path()).await;
        std::fs::create_dir(temp.path().join("hollow")).unwrap();

        assert!(storage
            .get_build_targets("hollow")
            .await
            .unwrap()
            .is_empty());
        assert!(!temp.path().join("hollow/targets.jsonl").exists());
    }

    #[tokio::test]
    async fn a_stream_that_ends_without_build_finished_is_interrupted() {
        use crate::proto::build::bazel::build_event_stream::Aborted;
        let temp = tempfile::TempDir::new().unwrap();
        let storage = storage_in(temp.path()).await;
        #[allow(deprecated)]
        let started = BuildEvent {
            payload: Some(Payload::Started(BuildStarted {
                uuid: "aborted".to_string(),
                command: "build".to_string(),
                start_time_millis: 1_790_000_000_000,
                ..Default::default()
            })),
            ..Default::default()
        };
        storage.store_event("aborted", started).await.unwrap();
        storage
            .store_event(
                "aborted",
                BuildEvent {
                    payload: Some(Payload::Aborted(Aborted::default())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        storage
            .store_event("aborted", last_message())
            .await
            .unwrap();

        let build = storage.get_build("aborted").await.unwrap().unwrap();
        assert_eq!(build.status, BuildStatus::Interrupted);
        assert!(build.start_time.is_some());
        assert!(build.end_time.is_some());
        let stats = storage.stats().await.unwrap();
        assert_eq!((stats.in_progress_builds, stats.interrupted_builds), (0, 1));
    }

    #[tokio::test]
    async fn label_histories_serve_target_detail_after_migration() {
        let temp = tempfile::TempDir::new().unwrap();
        let first = serde_json::to_string(&execution_for("//a:lib", "inv-1")).unwrap();
        let second = serde_json::to_string(&execution_for("//b:lib", "inv-2")).unwrap();
        std::fs::write(
            temp.path().join("targets.jsonl"),
            format!("{first}\n{second}\n"),
        )
        .unwrap();

        let storage = storage_in(temp.path()).await;
        storage.wait_label_index().await;
        assert!(temp.path().join("by-label/.complete").exists());
        assert!(storage.config.target_label_path("//a:lib").exists());

        storage
            .append_target_execution(&execution_for("//a:lib", "inv-3"))
            .await
            .unwrap();
        let history = storage.get_target_history("//a:lib").await.unwrap();
        let ids: Vec<&str> = history.iter().map(|e| e.invocation_id.as_str()).collect();
        assert_eq!(ids, vec!["inv-1", "inv-3"]);
        assert_eq!(
            storage.get_target_history("//b:lib").await.unwrap().len(),
            1
        );
        assert!(storage
            .get_target_history("//c:none")
            .await
            .unwrap()
            .is_empty());

        let again = storage_in(temp.path()).await;
        again.wait_label_index().await;
        assert_eq!(
            again.get_target_history("//a:lib").await.unwrap().len(),
            2,
            "a restart finds the sentinel and does not migrate again"
        );
    }

    #[tokio::test]
    async fn retention_compacts_label_histories() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut storage = storage_in(temp.path()).await;
        storage.wait_label_index().await;
        record_build(
            &storage,
            "old",
            "2020-01-01T10:00:00Z",
            "2020-01-01T10:01:00Z",
            true,
        )
        .await;
        record_build(
            &storage,
            "new",
            "2099-01-01T10:00:00Z",
            "2099-01-01T10:01:00Z",
            true,
        )
        .await;
        storage
            .append_target_execution(&execution_for("//a:lib", "old"))
            .await
            .unwrap();
        storage
            .append_target_execution(&execution_for("//a:lib", "new"))
            .await
            .unwrap();
        storage
            .append_target_execution(&execution_for("//gone:lib", "old"))
            .await
            .unwrap();

        storage.config.max_retention_days = Some(30);
        assert_eq!(storage.apply_retention().await.unwrap(), 1);
        let history = storage.get_target_history("//a:lib").await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].invocation_id, "new");
        assert!(!storage.config.target_label_path("//gone:lib").exists());
    }

    #[tokio::test]
    async fn targets_page_filters_and_counts() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = storage_in(temp.path()).await;
        for (label, id) in [("//a:lib", "i1"), ("//a:lib", "i2"), ("//b:bin", "i3")] {
            storage
                .append_target_execution(&execution_for(label, id))
                .await
                .unwrap();
        }
        let mut failed = execution_for("//c:test", "i4");
        failed.status = TargetExecutionStatus::Failure;
        storage.append_target_execution(&failed).await.unwrap();

        let page = storage.list_targets_page(10, 0, "", None).await.unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(page.items[0].label, "//a:lib");

        let page = storage.list_targets_page(1, 1, "", None).await.unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(page.items.len(), 1);

        let page = storage.list_targets_page(10, 0, "B:", None).await.unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].label, "//b:bin");

        let page = storage
            .list_targets_page(10, 0, "", Some(TargetExecutionStatus::Failure))
            .await
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].label, "//c:test");
    }

    #[tokio::test]
    async fn important_events_are_filtered_while_streaming() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = storage_in(temp.path()).await;
        std::fs::create_dir_all(temp.path().join("inv-x")).unwrap();
        let lines = [
            r#"{"kind":"Progress","payload":{"Progress":{"stdout":"","stderr":"x"}}}"#,
            r#"{"kind":"ActionExecuted","payload":{"ActionExecuted":{"success":true}}}"#,
            r#"{"kind":"ActionExecuted","payload":{"ActionExecuted":{"success":false}}}"#,
            r#"{"kind":"TargetComplete","payload":{"Completed":{"success":true}}}"#,
            r#"{"kind":"TestResult","payload":{"TestResult":{"status":1}}}"#,
            r#"{"kind":"TestResult","payload":{"TestResult":{"status":4}}}"#,
            r#"{"kind":"BuildFinished","payload":{"Finished":{"overall_success":false}}}"#,
        ];
        std::fs::write(
            temp.path().join("inv-x/events.jsonl"),
            lines.join("\n") + "\n",
        )
        .unwrap();

        let important = storage.get_events("inv-x", true).await.unwrap();
        let kinds: Vec<&str> = important
            .iter()
            .map(|e| e["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, vec!["ActionExecuted", "TestResult", "BuildFinished"]);
        assert_eq!(storage.get_events("inv-x", false).await.unwrap().len(), 7);
    }

    fn action_executed(
        mnemonic: &str,
        label: &str,
        start: &str,
        end: &str,
        success: bool,
    ) -> BuildEvent {
        BuildEvent {
            payload: Some(Payload::Action(ActionExecuted {
                success,
                r#type: mnemonic.to_string(),
                label: label.to_string(),
                start_time: Some(prost_types::Timestamp {
                    seconds: DateTime::parse_from_rfc3339(start).unwrap().timestamp(),
                    nanos: 0,
                }),
                end_time: Some(prost_types::Timestamp {
                    seconds: DateTime::parse_from_rfc3339(end).unwrap().timestamp(),
                    nanos: 0,
                }),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn action_timeline_ranks_mnemonics_by_total_duration_and_names_the_longest() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = storage_in(temp.path()).await;
        storage
            .store_event(
                "inv-1",
                action_executed(
                    "CppCompile",
                    "//a:one",
                    "2030-01-01T10:00:00Z",
                    "2030-01-01T10:00:05Z",
                    true,
                ),
            )
            .await
            .unwrap();
        storage
            .store_event(
                "inv-1",
                action_executed(
                    "CppCompile",
                    "//a:two",
                    "2030-01-01T10:00:05Z",
                    "2030-01-01T10:00:40Z",
                    false,
                ),
            )
            .await
            .unwrap();
        storage
            .store_event(
                "inv-1",
                action_executed(
                    "Genrule",
                    "//b:gen",
                    "2030-01-01T10:00:00Z",
                    "2030-01-01T10:00:02Z",
                    true,
                ),
            )
            .await
            .unwrap();

        let timeline = storage.get_action_timeline("inv-1").await.unwrap();
        assert_eq!(timeline.len(), 2);

        assert_eq!(timeline[0].mnemonic, "CppCompile");
        assert_eq!(timeline[0].action_count, 2);
        assert_eq!(timeline[0].failed_count, 1);
        assert_eq!(timeline[0].total_duration_ms, 5_000 + 35_000);
        assert_eq!(timeline[0].longest_label.as_deref(), Some("//a:two"));
        assert_eq!(timeline[0].longest_duration_ms, 35_000);
        assert_eq!(
            timeline[0].min_start.as_deref(),
            Some("2030-01-01T10:00:00+00:00")
        );
        assert_eq!(
            timeline[0].max_end.as_deref(),
            Some("2030-01-01T10:00:40+00:00")
        );

        assert_eq!(timeline[1].mnemonic, "Genrule");
        assert_eq!(timeline[1].action_count, 1);
        assert_eq!(timeline[1].failed_count, 0);
        assert_eq!(timeline[1].total_duration_ms, 2_000);
    }
}
