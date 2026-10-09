//! REAPI `Execution` service: schedules actions on remote workers and
//! streams their long-running operations back to Bazel.
//!
//! `Execute` answers a cache hit with a completed operation right away.
//! Otherwise it enqueues the action, streams `done: false` updates every
//! [`OPERATION_POLL_INTERVAL`], and finishes with the `ExecuteResponse` as
//! soon as the operation's state machine reaches a terminal state. An
//! identical action already in flight is joined rather than run twice.
//! `WaitExecution` attaches to an operation that is already running and
//! finishes with the same response.
//!
//! The response and the Action Cache entry are produced once per execution
//! by [`ActionRecorder`], which the execution engine runs before it
//! publishes the result, so they do not depend on any client still
//! listening.
//!
//! The response status is `OK` for every action that ran to completion,
//! whatever its exit code (which travels in the `ActionResult`). It is
//! `DEADLINE_EXCEEDED` when the worker killed the action at its timeout, and
//! `INTERNAL` when the action could not run at all. A successful result is written to the Action
//! Cache unless the action is `do_not_cache`.

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{debug, info, warn};

use crate::cache::action_cache::{ActionCacheStore, CacheActionResult};
use crate::cache::action_cache_log::{ActionCacheLog, ActionInputs, InputGroupStats};
use crate::cas::{CasError, CasResult, SharedCasBackend};
use crate::execution::engine::ResultRecorder;
use crate::execution::output_handler::{OutputHandler, OutputResult};
use crate::execution::results::ResultsStore;
use crate::execution::scheduler::{ExecutableAction, MultiLevelScheduler, QueuePriority};
use crate::execution::state_machine::{
    ExecutionStage, ExecutionStateMachine, OperationId, StateMachineManager,
};
use crate::metrics::Metrics;
use crate::proto::build::bazel::remote::execution::v2::{
    execution_server::{Execution, ExecutionServer},
    Action, ActionResult, Command, Digest, ExecuteRequest, ExecuteResponse, ExecutedActionMetadata,
    OutputDirectory, OutputFile, OutputSymlink, Platform, RequestMetadata, WaitExecutionRequest,
};
use crate::proto::google::longrunning::{operation::Result as OpResult, Operation};
use crate::proto::google::rpc::Status as RpcStatus;
use crate::server::action_cache_service::ResultVerifier;
use crate::server::capabilities_service::PRIORITY_RANGE;
use crate::server::critical_actions::CriticalActions;
use crate::server::request_metadata::request_metadata;
use crate::types::DigestInfo;
use crate::worker::k8s::{
    ExecutionResult, InputStats, NetworkPolicy, OutputDirectory as WorkerOutputDirectory,
    OutputFile as WorkerOutputFile, OutputSymlink as WorkerOutputSymlink, PhaseTimestamps,
    WorkerRequirements,
};

use prost::Message as ProstMessage;
use prost_types::Any;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How often a running operation is polled and a progress update streamed.
const OPERATION_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Timeout applied to actions that declare none (or a non-positive one),
/// overridable with `RBE_DEFAULT_ACTION_TIMEOUT_SECS`. Bazel sets
/// `Action.timeout` only for tests, so this caps every remote build action;
/// it stays below `STALE_ACTION_AGE` so the action is killed, not dropped.
const DEFAULT_ACTION_TIMEOUT: Duration = Duration::from_secs(1800);

/// Operation updates buffered per client stream.
const OPERATION_CHANNEL_DEPTH: usize = 10;

/// Default gRPC message limit in both directions, overridable with
/// `RBE_MAX_GRPC_MSG_SIZE` (bytes).
const DEFAULT_MAX_GRPC_MSG_SIZE: usize = 100 * 1024 * 1024;

const EXECUTE_RESPONSE_TYPE_URL: &str =
    "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse";

/// `google.rpc.Code.DEADLINE_EXCEEDED`.
const RPC_DEADLINE_EXCEEDED: i32 = 4;

/// `google.rpc.Code.INTERNAL`.
const RPC_INTERNAL: i32 = 13;

/// The `OK` status of an action that ran; the exit code is not a status.
fn ok_status() -> Option<RpcStatus> {
    Some(RpcStatus {
        code: 0,
        message: String::new(),
        details: vec![],
    })
}

/// The status of a worker result: `INTERNAL` with the worker's reason when
/// the action could not run, `DEADLINE_EXCEEDED` when it was killed at its
/// timeout, `OK` otherwise.
fn result_status(result: &ExecutionResult) -> Option<RpcStatus> {
    let (code, message) = match (&result.error, result.timed_out) {
        (Some(error), _) => (RPC_INTERNAL, error.clone()),
        (None, true) => (
            RPC_DEADLINE_EXCEEDED,
            "action timed out and was killed".to_string(),
        ),
        (None, false) => return ok_status(),
    };
    Some(RpcStatus {
        code,
        message,
        details: vec![],
    })
}

/// An `ExecuteResponse` packed as the operation's result.
fn pack_response(response: &ExecuteResponse) -> OpResult {
    OpResult::Response(Any {
        type_url: EXECUTE_RESPONSE_TYPE_URL.to_string(),
        value: response.encode_to_vec(),
    })
}

fn operation(name: String, result: Option<OpResult>) -> Operation {
    Operation {
        name,
        metadata: None,
        done: result.is_some(),
        result,
    }
}

fn proto_output_files(files: &[WorkerOutputFile]) -> Vec<OutputFile> {
    files
        .iter()
        .map(|f| OutputFile {
            path: f.path.clone(),
            digest: Some(Digest {
                hash: f.digest.clone(),
                size_bytes: f.size_bytes,
            }),
            is_executable: f.is_executable,
            contents: vec![],
            node_properties: None,
        })
        .collect()
}

fn proto_output_directories(directories: &[WorkerOutputDirectory]) -> Vec<OutputDirectory> {
    directories
        .iter()
        .map(|d| OutputDirectory {
            path: d.path.clone(),
            tree_digest: Some(Digest {
                hash: d.digest.clone(),
                size_bytes: d.size_bytes,
            }),
            is_topologically_sorted: false,
            root_directory_digest: None,
        })
        .collect()
}

/// Symlink outputs in `output_symlinks`, the field for clients that send
/// `output_paths` (Bazel 7+); the deprecated per-kind fields stay empty.
fn proto_output_symlinks(symlinks: &[WorkerOutputSymlink]) -> Vec<OutputSymlink> {
    symlinks
        .iter()
        .map(|s| OutputSymlink {
            path: s.path.clone(),
            target: s.target.clone(),
            node_properties: None,
        })
        .collect()
}

/// A protobuf timestamp for Unix milliseconds; `None` for `0`, a phase that
/// did not happen.
fn timestamp_ms(ms: i64) -> Option<prost_types::Timestamp> {
    (ms > 0).then(|| prost_types::Timestamp {
        seconds: ms.div_euclid(1000),
        nanos: (ms.rem_euclid(1000) * 1_000_000) as i32,
    })
}

/// Records a cache hit served by `Execute` like one served by
/// `GetActionResult`; a request without invocation or target is not
/// attributed.
async fn record_cache_hit(log: &ActionCacheLog, caller: &RequestMetadata) {
    if caller.tool_invocation_id.is_empty() || caller.target_id.is_empty() {
        return;
    }
    log.record(
        &caller.tool_invocation_id,
        &caller.target_id,
        &caller.action_mnemonic,
        true,
    )
    .await;
}

/// Records what an execution materialized against the target of its
/// caller; a request without invocation or target is not attributed.
async fn record_inputs(log: &ActionCacheLog, caller: &RequestMetadata, stats: &InputStats) {
    if caller.tool_invocation_id.is_empty() || caller.target_id.is_empty() {
        return;
    }
    let inputs = ActionInputs {
        mnemonic: caller.action_mnemonic.clone(),
        files: stats.files,
        bytes: stats.bytes,
        groups: stats
            .groups
            .iter()
            .map(|g| InputGroupStats {
                name: g.name.clone(),
                files: g.files,
                bytes: g.bytes,
            })
            .collect(),
    };
    log.record_inputs(&caller.tool_invocation_id, &caller.target_id, inputs)
        .await;
}

/// Records which worker produced the outputs of an execution, by exec path,
/// for the caller's invocation; a joined execution counts for each caller.
async fn record_placement(
    log: &ActionCacheLog,
    caller: &RequestMetadata,
    working_directory: &str,
    result: &ExecutionResult,
) {
    if caller.tool_invocation_id.is_empty() || result.worker_id.is_empty() {
        return;
    }
    let exec_path = |path: &str| match working_directory {
        "" => path.to_string(),
        dir => format!("{}/{}", dir.trim_end_matches('/'), path),
    };
    let outputs: Vec<String> = result
        .output_files
        .iter()
        .map(|f| exec_path(&f.path))
        .chain(result.output_directories.iter().map(|d| exec_path(&d.path)))
        .chain(result.output_symlinks.iter().map(|s| exec_path(&s.path)))
        .collect();
    log.record_placement(
        &caller.tool_invocation_id,
        &result.worker_id,
        outputs.iter().map(String::as_str),
    )
    .await;
}

/// The REAPI metadata of an execution: the worker, its phase timestamps and
/// `queued_at`, when the operation was created.
fn execution_metadata(
    result: &ExecutionResult,
    queued_at: Option<SystemTime>,
) -> ExecutedActionMetadata {
    let t: &PhaseTimestamps = &result.timestamps;
    ExecutedActionMetadata {
        worker: result.worker_id.clone(),
        queued_timestamp: queued_at.map(prost_types::Timestamp::from),
        worker_start_timestamp: timestamp_ms(t.worker_start_ms),
        worker_completed_timestamp: timestamp_ms(t.worker_completed_ms),
        input_fetch_start_timestamp: timestamp_ms(t.input_fetch_start_ms),
        input_fetch_completed_timestamp: timestamp_ms(t.input_fetch_completed_ms),
        execution_start_timestamp: timestamp_ms(t.execution_start_ms),
        execution_completed_timestamp: timestamp_ms(t.execution_completed_ms),
        output_upload_start_timestamp: timestamp_ms(t.output_upload_start_ms),
        output_upload_completed_timestamp: timestamp_ms(t.output_upload_completed_ms),
        ..Default::default()
    }
}

/// How long an execution spent in each phase it went through, from the
/// worker's timestamps and `queued_at`; a phase missing either end, or
/// running backwards across clocks, is left out.
fn phase_durations(
    result: &ExecutionResult,
    queued_at: Option<SystemTime>,
) -> Vec<(&'static str, Duration)> {
    let t = &result.timestamps;
    let queued_ms = queued_at
        .and_then(|q| q.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64);
    [
        ("queued", queued_ms, t.worker_start_ms),
        (
            "input_fetch",
            t.input_fetch_start_ms,
            t.input_fetch_completed_ms,
        ),
        ("execution", t.execution_start_ms, t.execution_completed_ms),
        (
            "output_upload",
            t.output_upload_start_ms,
            t.output_upload_completed_ms,
        ),
    ]
    .into_iter()
    .filter(|&(_, start, end)| start > 0 && end >= start)
    .map(|(phase, start, end)| (phase, Duration::from_millis((end - start) as u64)))
    .collect()
}

/// When the operation behind `sm` was created, on the wall clock.
fn queued_at(sm: &ExecutionStateMachine) -> Option<SystemTime> {
    SystemTime::now()
        .checked_sub(sm.elapsed())
        .filter(|t| *t > UNIX_EPOCH)
}

/// The `ActionResult` of a worker execution with the given stdout/stderr
/// representation (inline bytes or CAS digests).
#[allow(deprecated)]
fn worker_action_result(
    result: &ExecutionResult,
    queued_at: Option<SystemTime>,
    stdout_raw: Vec<u8>,
    stdout_digest: Option<Digest>,
    stderr_raw: Vec<u8>,
    stderr_digest: Option<Digest>,
) -> ActionResult {
    ActionResult {
        output_files: proto_output_files(&result.output_files),
        output_directories: proto_output_directories(&result.output_directories),
        exit_code: result.exit_code,
        stdout_raw,
        stdout_digest,
        stderr_raw,
        stderr_digest,
        execution_metadata: Some(execution_metadata(result, queued_at)),
        output_file_symlinks: vec![],
        output_symlinks: proto_output_symlinks(&result.output_symlinks),
        output_directory_symlinks: vec![],
    }
}

fn completed_response(action_result: ActionResult, result: &ExecutionResult) -> ExecuteResponse {
    let status = result_status(result);
    ExecuteResponse {
        result: Some(action_result),
        cached_result: false,
        message: status
            .as_ref()
            .map(|s| s.message.clone())
            .unwrap_or_default(),
        status,
        server_logs: HashMap::new(),
    }
}

/// The response for an operation that reached a terminal state without a
/// recorded result.
fn missing_result_response() -> ExecuteResponse {
    ExecuteResponse {
        result: None,
        cached_result: false,
        status: Some(RpcStatus {
            code: 2,
            message: "Execution result not found".to_string(),
            details: vec![],
        }),
        message: "Execution result not found".to_string(),
        server_logs: HashMap::new(),
    }
}

/// Inline bytes when `output` stayed inline, empty otherwise.
fn inline_bytes(output: &OutputResult) -> Vec<u8> {
    if output.is_inline() {
        output.raw.clone().unwrap_or_default()
    } else {
        Vec::new()
    }
}

fn output_digest(output: &OutputResult) -> Option<Digest> {
    output.digest.map(|d| Digest {
        hash: d.hash_to_string(),
        size_bytes: d.size,
    })
}

/// The non-empty names of a comma-separated list, trimmed.
fn parse_property_names(names: &str) -> Vec<String> {
    names
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// The `(name, value)` pairs of a REAPI platform, empty when there is none.
fn platform_properties(platform: Option<&Platform>) -> Vec<(String, String)> {
    platform
        .map(|p| {
            p.properties
                .iter()
                .map(|property| (property.name.clone(), property.value.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// The action digest of a request, or `INVALID_ARGUMENT`.
fn action_digest_of(digest: Option<Digest>) -> Result<DigestInfo, Status> {
    let digest = digest.ok_or_else(|| Status::invalid_argument("Missing action_digest"))?;
    DigestInfo::new(&digest.hash, digest.size_bytes)
        .map_err(|e| Status::invalid_argument(format!("Invalid action_digest: {}", e)))
}

/// Writes the result of an action that ran and exited `0` to the Action
/// Cache, so the next `Execute` or `GetActionResult` of the same digest hits.
async fn cache_success(
    action_cache: &dyn ActionCacheStore,
    digest: DigestInfo,
    result: &ExecutionResult,
    response: &ExecuteResponse,
) {
    if result.exit_code != 0 || result.error.is_some() || result.timed_out {
        return;
    }
    if let Some(action_result) = &response.result {
        action_cache
            .put(
                digest,
                CacheActionResult::new(digest, action_result.clone()),
            )
            .await;
        debug!("Cached result of {}", digest);
    }
}

/// The [`ResultRecorder`] of the server. For each finished execution it
/// builds the `ExecuteResponse` (stdout and stderr inline, or in the CAS when
/// large), writes a clean success to the Action Cache unless the action is
/// `do_not_cache`, and records the execution's phases and inputs.
pub struct ActionRecorder {
    action_cache: Arc<dyn ActionCacheStore>,
    output_handler: OutputHandler,
    metrics: Option<Arc<Metrics>>,
    /// Where the inputs of each execution are recorded per target.
    action_cache_log: Option<Arc<ActionCacheLog>>,
}

impl ActionRecorder {
    pub fn new(action_cache: Arc<dyn ActionCacheStore>, cas_backend: SharedCasBackend) -> Self {
        Self {
            action_cache,
            output_handler: OutputHandler::new(cas_backend),
            metrics: None,
            action_cache_log: None,
        }
    }

    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Records the inputs each execution materialized against the target and
    /// invocation of the `RequestMetadata` that enqueued it.
    pub fn with_action_cache_log(mut self, log: Arc<ActionCacheLog>) -> Self {
        self.action_cache_log = Some(log);
        self
    }

    /// The `ExecuteResponse` for a finished worker execution. Large stdout
    /// and stderr are stored in the CAS and referenced by digest, small ones
    /// are inlined; if storing fails the output is inlined instead.
    async fn response(
        &self,
        result: &ExecutionResult,
        queued_at: Option<SystemTime>,
    ) -> ExecuteResponse {
        let stdout_result = self
            .output("stdout", &result.stdout, result.stdout_digest)
            .await;
        let stderr_result = self
            .output("stderr", &result.stderr, result.stderr_digest)
            .await;

        debug!(
            "Output handling: stdout_inline={}, stdout_stored={}, stderr_inline={}, stderr_stored={}",
            stdout_result.is_inline(),
            stdout_result.is_stored(),
            stderr_result.is_inline(),
            stderr_result.is_stored()
        );

        let action_result = worker_action_result(
            result,
            queued_at,
            inline_bytes(&stdout_result),
            output_digest(&stdout_result),
            inline_bytes(&stderr_result),
            output_digest(&stderr_result),
        );
        completed_response(action_result, result)
    }

    /// One stream of the response: the CAS digest the worker stored it
    /// under, or its inline bytes through the [`OutputHandler`].
    async fn output(&self, name: &str, bytes: &[u8], stored: Option<DigestInfo>) -> OutputResult {
        if let Some(digest) = stored {
            return OutputResult::stored(digest);
        }
        self.output_handler
            .process_output(name, bytes.to_vec())
            .await
            .unwrap_or_else(|e| {
                warn!("Failed to process {}: {}, using inline", name, e);
                OutputResult::inline(bytes.to_vec())
            })
    }
}

#[async_trait::async_trait]
impl ResultRecorder for ActionRecorder {
    async fn record(
        &self,
        action: Option<&ExecutableAction>,
        state_machine: &ExecutionStateMachine,
        result: &ExecutionResult,
    ) -> ExecuteResponse {
        let queued_at = queued_at(state_machine);
        let response = self.response(result, queued_at).await;
        if let Some(action) = action.filter(|action| !action.do_not_cache) {
            cache_success(&*self.action_cache, action.action_digest, result, &response).await;
        }
        let caller = action.and_then(|action| action.metadata.as_ref());
        if let Some(metrics) = &self.metrics {
            for (phase, duration) in phase_durations(result, queued_at) {
                metrics.record_execution_phase(phase, duration);
            }
            if let Some(stats) = &result.input_stats {
                let mnemonic = caller.map_or("", |m| m.action_mnemonic.as_str());
                metrics.record_execution_inputs(mnemonic, stats.files, stats.bytes);
            }
        }
        if let (Some(log), Some(caller), Some(stats)) =
            (&self.action_cache_log, caller, &result.input_stats)
        {
            record_inputs(log, caller, stats).await;
        }
        response
    }
}

/// Sends `done: false` updates of `name` every [`OPERATION_POLL_INTERVAL`]
/// until the operation behind `sm` reaches a terminal stage, and returns
/// whether the client is still there to receive its outcome.
async fn until_finished(
    sm: &ExecutionStateMachine,
    name: &str,
    tx: &mpsc::Sender<Result<Operation, Status>>,
) -> bool {
    let _watch = sm.watch();
    let mut keepalive = tokio::time::interval(OPERATION_POLL_INTERVAL);
    keepalive.tick().await;
    loop {
        tokio::select! {
            () = sm.wait_terminal() => return true,
            () = tx.closed() => return false,
            _ = keepalive.tick() => {
                if tx.send(Ok(operation(name.to_string(), None))).await.is_err() {
                    return false;
                }
            }
        }
    }
}

/// `Execution` over the scheduler, worker results and the Action Cache.
pub struct ExecutionService {
    scheduler: Arc<MultiLevelScheduler>,
    state_manager: Arc<StateMachineManager>,
    l1_cache: Arc<dyn ActionCacheStore>,
    results_store: Arc<ResultsStore>,
    cas_backend: SharedCasBackend,
    critical_actions: CriticalActions,
    default_action_timeout: Duration,
    metrics: Option<Arc<Metrics>>,
    /// Where the worker of each execution is recorded per invocation.
    action_cache_log: Option<Arc<ActionCacheLog>>,
    /// Checks a cache hit's outputs before `Execute` serves it.
    verifier: Arc<ResultVerifier>,
    /// Platform properties accepted beyond the known ones, from
    /// `RBE_EXTRA_PLATFORM_PROPERTIES` (comma-separated names); see
    /// [`WorkerRequirements::from_platform_with`].
    extra_platform_properties: Vec<String>,
}

impl ExecutionService {
    pub fn new(
        scheduler: Arc<MultiLevelScheduler>,
        state_manager: Arc<StateMachineManager>,
        l1_cache: Arc<dyn ActionCacheStore>,
        results_store: Arc<ResultsStore>,
        cas_backend: SharedCasBackend,
    ) -> Self {
        Self {
            scheduler,
            state_manager,
            l1_cache,
            results_store,
            verifier: Arc::new(ResultVerifier::new(cas_backend.clone())),
            cas_backend,
            critical_actions: CriticalActions::default(),
            default_action_timeout: std::env::var("RBE_DEFAULT_ACTION_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .filter(|&secs| secs > 0)
                .map(Duration::from_secs)
                .unwrap_or(DEFAULT_ACTION_TIMEOUT),
            metrics: None,
            action_cache_log: None,
            extra_platform_properties: std::env::var("RBE_EXTRA_PLATFORM_PROPERTIES")
                .map(|names| parse_property_names(&names))
                .unwrap_or_default(),
        }
    }

    /// Accepts the platform properties `names` beyond the known ones.
    pub fn with_extra_platform_properties(mut self, names: Vec<String>) -> Self {
        self.extra_platform_properties = names;
        self
    }

    /// Records which worker produced the outputs of each execution, per
    /// invocation of every caller, joined ones included.
    pub fn with_action_cache_log(mut self, log: Arc<ActionCacheLog>) -> Self {
        self.action_cache_log = Some(log);
        self
    }

    /// Shares the cache-hit verifier (and its verified trees) with the
    /// `ActionCache` service.
    pub fn with_verifier(mut self, verifier: Arc<ResultVerifier>) -> Self {
        self.verifier = verifier;
        self
    }

    /// Counts the cache hits `Execute` drops because their outputs left the
    /// CAS in `metrics`.
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Promotes the `critical_actions` to the critical path by their
    /// `RequestMetadata` (see [`CriticalActions::properties_for`]).
    pub fn with_critical_actions(mut self, critical_actions: CriticalActions) -> Self {
        self.critical_actions = critical_actions;
        self
    }

    /// The tonic server for this service, with message limits of
    /// [`DEFAULT_MAX_GRPC_MSG_SIZE`] unless `RBE_MAX_GRPC_MSG_SIZE` is set.
    pub fn into_service(self) -> ExecutionServer<Self> {
        let max_msg_size = std::env::var("RBE_MAX_GRPC_MSG_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_GRPC_MSG_SIZE);
        ExecutionServer::new(self)
            .max_decoding_message_size(max_msg_size)
            .max_encoding_message_size(max_msg_size)
    }

    /// The schedulable form of a request: its `Action` and `Command` are read
    /// from the CAS, which must already hold both (`NOT_FOUND` otherwise).
    ///
    /// Outputs come from `Command.output_paths` when set, and from the
    /// deprecated `output_files`/`output_directories` otherwise. The worker
    /// requirements come from `Action.platform`, or from the deprecated
    /// `Command.platform` when the action has none, plus whatever the
    /// configured critical actions add for its `metadata`; an unsupported
    /// platform is `INVALID_ARGUMENT`.
    async fn create_executable_action(
        &self,
        req: ExecuteRequest,
        metadata: Option<&RequestMetadata>,
        operation_id: OperationId,
    ) -> Result<ExecutableAction, Status> {
        let action_digest = action_digest_of(req.action_digest.clone())?;
        let execution_priority = req.execution_policy.as_ref().map_or(0, |policy| {
            policy
                .priority
                .clamp(PRIORITY_RANGE.min_priority, PRIORITY_RANGE.max_priority)
        });

        let action = match self.fetch_action(&action_digest).await {
            Ok(Some(action)) => action,
            Ok(None) => {
                warn!(
                    "Action not found in CAS: {}",
                    action_digest.hash_to_string()
                );
                return Err(Status::not_found(format!(
                    "Action not found: {}",
                    action_digest.hash_to_string()
                )));
            }
            Err(e) => {
                warn!("Failed to fetch action from CAS: {}", e);
                return Err(Status::internal("Failed to fetch action"));
            }
        };

        let command_digest = action
            .command_digest
            .ok_or_else(|| Status::invalid_argument("Action missing command_digest"))?;
        let command_digest_info = DigestInfo::new(&command_digest.hash, command_digest.size_bytes)
            .map_err(|e| Status::invalid_argument(format!("Invalid command_digest: {}", e)))?;

        let command = match self.fetch_command(&command_digest_info).await {
            Ok(Some(cmd)) => cmd,
            Ok(None) => {
                warn!(
                    "Command not found in CAS: {}",
                    command_digest_info.hash_to_string()
                );
                return Err(Status::not_found(format!(
                    "Command not found: {}",
                    command_digest_info.hash_to_string()
                )));
            }
            Err(e) => {
                warn!("Failed to fetch command from CAS: {}", e);
                return Err(Status::internal("Failed to fetch command"));
            }
        };

        let input_root_digest = action
            .input_root_digest
            .map(|d| DigestInfo::new(&d.hash, d.size_bytes))
            .transpose()
            .map_err(|e| Status::invalid_argument(format!("Invalid input_root_digest: {}", e)))?;

        let (output_files, output_directories) = if !command.output_paths.is_empty() {
            (command.output_paths.clone(), vec![])
        } else {
            #[allow(deprecated)]
            (command.output_files, command.output_directories)
        };

        #[allow(deprecated)]
        let platform = action
            .platform
            .as_ref()
            .filter(|p| !p.properties.is_empty())
            .or(command.platform.as_ref());
        let mut properties = platform_properties(platform);
        let critical = self.critical_actions.properties_for(metadata, &properties);
        if !critical.is_empty() {
            debug!("Critical action {}: adding {:?}", action_digest, critical);
            properties.extend(critical);
        }
        let requirements =
            WorkerRequirements::from_platform_with(&properties, &self.extra_platform_properties)
                .map_err(|e| Status::invalid_argument(format!("Unsupported platform: {e}")))?;
        let priority = QueuePriority::from_platform(&properties)
            .map_err(|e| Status::invalid_argument(format!("Unsupported platform: {e}")))?;
        let network = NetworkPolicy::from_platform(&properties)
            .map_err(|e| Status::invalid_argument(format!("Unsupported platform: {e}")))?;

        let timeout = action
            .timeout
            .filter(|d| d.seconds > 0)
            .map(|d| Duration::from_secs(d.seconds as u64))
            .unwrap_or(self.default_action_timeout);

        Ok(ExecutableAction {
            operation_id,
            action_digest,
            command: command.arguments,
            timeout,
            priority,
            output_files,
            output_directories,
            working_directory: Some(command.working_directory).filter(|w| !w.is_empty()),
            input_root_digest,
            environment_variables: command
                .environment_variables
                .into_iter()
                .map(|v| (v.name, v.value))
                .collect(),
            do_not_cache: action.do_not_cache,
            requirements,
            network,
            worker_losses: 0,
            metadata: metadata.cloned(),
            execution_priority,
        })
    }

    async fn fetch_action(&self, digest: &DigestInfo) -> CasResult<Option<Action>> {
        let Some(bytes) = self.cas_backend.read(digest).await? else {
            return Ok(None);
        };
        Action::decode(bytes)
            .map(Some)
            .map_err(|e| CasError::InvalidData(format!("Failed to decode Action: {}", e)))
    }

    async fn fetch_command(&self, digest: &DigestInfo) -> CasResult<Option<Command>> {
        let Some(bytes) = self.cas_backend.read(digest).await? else {
            return Ok(None);
        };
        Command::decode(bytes)
            .map(Some)
            .map_err(|e| CasError::InvalidData(format!("Failed to decode Command: {}", e)))
    }

    /// The completed operation for a cached `ActionResult`.
    fn cached_operation(digest: &DigestInfo, cached: ActionResult) -> Operation {
        let response = ExecuteResponse {
            result: Some(cached),
            cached_result: true,
            status: ok_status(),
            message: String::new(),
            server_logs: HashMap::new(),
        };
        operation(
            format!("operations/{}", digest),
            Some(pack_response(&response)),
        )
    }
}

#[tonic::async_trait]
impl Execution for ExecutionService {
    type ExecuteStream = ReceiverStream<Result<Operation, Status>>;

    async fn execute(
        &self,
        request: Request<ExecuteRequest>,
    ) -> Result<Response<ReceiverStream<Result<Operation, Status>>>, Status> {
        let metadata = request_metadata(&request);
        let req = request.into_inner();
        let digest_info = action_digest_of(req.action_digest.clone())?;

        info!("Execution::Execute digest={}", digest_info);

        let (tx, rx) = tokio::sync::mpsc::channel(OPERATION_CHANNEL_DEPTH);

        if !req.skip_cache_lookup {
            if let Some(cached_result) = self.l1_cache.get(&digest_info).await {
                match self.verifier.missing_blobs(&cached_result.proto).await {
                    Ok(0) => {
                        info!("Cache hit for action {}", digest_info);
                        if let (Some(log), Some(caller)) = (&self.action_cache_log, &metadata) {
                            record_cache_hit(log, caller).await;
                        }
                        let op = Self::cached_operation(&digest_info, cached_result.proto);
                        let _ = tx.send(Ok(op)).await;
                        return Ok(Response::new(ReceiverStream::new(rx)));
                    }
                    Ok(missing) => {
                        warn!(
                            "Cache hit for action {} references {} blob(s) no longer in the CAS; executing it",
                            digest_info, missing
                        );
                        if let Some(metrics) = &self.metrics {
                            metrics.record_action_cache_stale_hit();
                        }
                        self.l1_cache.remove(&digest_info).await;
                    }
                    Err(e) => warn!(
                        "Could not verify the cache hit for action {}: {}; executing it",
                        digest_info, e
                    ),
                }
            }
        }

        let operation_id = OperationId::generate();
        let sm = self
            .state_manager
            .create_machine_with_id(operation_id, digest_info);
        let action = match self
            .create_executable_action(req, metadata.as_ref(), operation_id)
            .await
        {
            Ok(action) => action,
            Err(e) => {
                self.state_manager.remove_machine(operation_id);
                let _ = tx.send(Err(e)).await;
                return Ok(Response::new(ReceiverStream::new(rx)));
            }
        };
        if let Err(e) = sm.transition_to(ExecutionStage::Queued).await {
            self.state_manager.remove_machine(operation_id);
            warn!("Operation {} cannot be queued: {}", operation_id, e);
            let _ = tx
                .send(Err(Status::internal("Failed to queue action")))
                .await;
            return Ok(Response::new(ReceiverStream::new(rx)));
        }

        let working_directory = action.working_directory.clone().unwrap_or_default();
        let owner = match self.scheduler.enqueue(action, sm.clone()) {
            Ok(owner) => owner,
            Err(e) => {
                self.state_manager.remove_machine(operation_id);
                warn!("Failed to enqueue action: {}", e);
                let _ = tx
                    .send(Err(Status::internal("Failed to enqueue action")))
                    .await;
                return Ok(Response::new(ReceiverStream::new(rx)));
            }
        };
        let (operation_id, sm) = if owner == operation_id {
            info!("Enqueued operation {}", operation_id);
            (operation_id, sm)
        } else {
            self.state_manager.remove_machine(operation_id);
            match self.state_manager.get_machine(owner) {
                Some(existing) => {
                    info!("Joined in-flight operation {} for {}", owner, digest_info);
                    (owner, existing)
                }
                None => {
                    warn!("In-flight operation {} has no state machine", owner);
                    let _ = tx
                        .send(Err(Status::internal("In-flight operation lost")))
                        .await;
                    return Ok(Response::new(ReceiverStream::new(rx)));
                }
            }
        };

        let name = format!("operations/{}", operation_id.0);
        let _ = tx.send(Ok(operation(name.clone(), None))).await;

        let results_store = self.results_store.clone();
        let placement_log = self.action_cache_log.clone();
        tokio::spawn(async move {
            if !until_finished(&sm, &name, &tx).await {
                return;
            }
            let response = match results_store.get(operation_id) {
                Some(finished) => {
                    if let (Some(log), Some(caller)) = (&placement_log, &metadata) {
                        record_placement(log, caller, &working_directory, &finished.result).await;
                    }
                    finished.response
                }
                None => {
                    warn!("No result found for operation {}", operation_id.0);
                    missing_result_response()
                }
            };
            let _ = tx
                .send(Ok(operation(name, Some(pack_response(&response)))))
                .await;
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    type WaitExecutionStream = ReceiverStream<Result<Operation, Status>>;

    /// Streams updates of an operation started by `Execute`, named
    /// `operations/<id>`, and finishes with the same `ExecuteResponse`
    /// `Execute` sends; `NOT_FOUND` when no such operation is tracked.
    async fn wait_execution(
        &self,
        request: Request<WaitExecutionRequest>,
    ) -> Result<Response<ReceiverStream<Result<Operation, Status>>>, Status> {
        let req = request.into_inner();
        info!("Execution::WaitExecution name={}", req.name);

        let (tx, rx) = tokio::sync::mpsc::channel(OPERATION_CHANNEL_DEPTH);

        let op_id: u64 = req
            .name
            .trim_start_matches("operations/")
            .parse()
            .map_err(|_| Status::invalid_argument("Invalid operation name"))?;
        let op_id = OperationId(op_id);
        let results_store = self.results_store.clone();

        let Some(sm) = self.state_manager.get_machine(op_id) else {
            let _ = tx.send(Err(Status::not_found("Operation not found"))).await;
            return Ok(Response::new(ReceiverStream::new(rx)));
        };

        tokio::spawn(async move {
            if !until_finished(&sm, &req.name, &tx).await {
                return;
            }
            let response = match results_store.get(op_id) {
                Some(finished) => finished.response,
                None => {
                    warn!("No result found for operation {}", op_id.0);
                    missing_result_response()
                }
            };
            let _ = tx
                .send(Ok(operation(req.name, Some(pack_response(&response)))))
                .await;
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::action_cache::L1ActionCache;
    use crate::cas::backends::DiskBackend;
    use crate::cas::CasBackend;
    use crate::proto::build::bazel::remote::execution::v2::command::EnvironmentVariable;
    use bytes::Bytes;
    use tempfile::TempDir;
    use tokio_stream::StreamExt;

    fn digest_of(data: &[u8]) -> (DigestInfo, Digest) {
        use sha2::{Digest as _, Sha256};
        let hash = hex::encode(Sha256::digest(data));
        let info = DigestInfo::new(&hash, data.len() as i64).unwrap();
        (
            info,
            Digest {
                hash,
                size_bytes: data.len() as i64,
            },
        )
    }

    struct Fixture {
        _temp: TempDir,
        cas: Arc<DiskBackend>,
        action_cache: Arc<L1ActionCache>,
        service: ExecutionService,
    }

    async fn fixture() -> Fixture {
        let temp = TempDir::new().unwrap();
        let cas = Arc::new(DiskBackend::new(temp.path()).await.unwrap());
        let action_cache = Arc::new(L1ActionCache::default());
        let service = ExecutionService::new(
            Arc::new(MultiLevelScheduler::new()),
            Arc::new(StateMachineManager::new()),
            action_cache.clone(),
            Arc::new(ResultsStore::new()),
            cas.clone(),
        );
        Fixture {
            _temp: temp,
            cas,
            action_cache,
            service,
        }
    }

    /// Stores a `Command` and an `Action` pointing at it, and returns the
    /// action digest.
    async fn store_action(cas: &DiskBackend, command: Command, do_not_cache: bool) -> Digest {
        store_action_on(cas, command, do_not_cache, None).await
    }

    async fn store_action_on(
        cas: &DiskBackend,
        command: Command,
        do_not_cache: bool,
        platform: Option<Platform>,
    ) -> Digest {
        let command_bytes = command.encode_to_vec();
        let (command_info, command_digest) = digest_of(&command_bytes);
        cas.write(&command_info, Bytes::from(command_bytes))
            .await
            .unwrap();
        let action = Action {
            command_digest: Some(command_digest),
            do_not_cache,
            platform,
            ..Default::default()
        };
        let action_bytes = action.encode_to_vec();
        let (action_info, action_digest) = digest_of(&action_bytes);
        cas.write(&action_info, Bytes::from(action_bytes))
            .await
            .unwrap();
        action_digest
    }

    fn execute_request(action_digest: Digest) -> ExecuteRequest {
        ExecuteRequest {
            action_digest: Some(action_digest),
            ..Default::default()
        }
    }

    fn worker_result(exit_code: i32, error: Option<&str>) -> ExecutionResult {
        ExecutionResult {
            execution_id: "exec-1".to_string(),
            exit_code,
            stdout: vec![],
            stderr: vec![],
            stdout_digest: None,
            stderr_digest: None,
            output_files: vec![],
            output_directories: vec![],
            output_symlinks: vec![],
            execution_duration: Duration::ZERO,
            error: error.map(str::to_string),
            timed_out: false,
            worker_id: String::new(),
            timestamps: PhaseTimestamps::default(),
            input_stats: None,
        }
    }

    #[tokio::test]
    async fn the_command_environment_working_directory_and_do_not_cache_reach_the_action() {
        let f = fixture().await;
        let command = Command {
            arguments: vec!["/bin/bash".into(), "-c".into(), "true".into()],
            environment_variables: vec![
                EnvironmentVariable {
                    name: "LANG".into(),
                    value: "C.UTF-8".into(),
                },
                EnvironmentVariable {
                    name: "PATH".into(),
                    value: "/bin".into(),
                },
            ],
            working_directory: "pkg/sub".into(),
            ..Default::default()
        };
        let digest = store_action(&f.cas, command, true).await;

        let action = f
            .service
            .create_executable_action(execute_request(digest), None, OperationId::generate())
            .await
            .unwrap();

        assert_eq!(action.working_directory.as_deref(), Some("pkg/sub"));
        assert_eq!(
            action.environment_variables,
            vec![
                ("LANG".to_string(), "C.UTF-8".to_string()),
                ("PATH".to_string(), "/bin".to_string()),
            ]
        );
        assert!(action.do_not_cache);
    }

    #[tokio::test]
    async fn an_action_without_timeout_gets_the_default_one() {
        let f = fixture().await;
        let digest = store_action(&f.cas, Command::default(), false).await;
        let action = f
            .service
            .create_executable_action(execute_request(digest), None, OperationId::generate())
            .await
            .unwrap();
        assert_eq!(action.timeout, DEFAULT_ACTION_TIMEOUT);
    }

    #[tokio::test]
    async fn an_empty_working_directory_means_the_input_root() {
        let f = fixture().await;
        let digest = store_action(&f.cas, Command::default(), false).await;
        let action = f
            .service
            .create_executable_action(execute_request(digest), None, OperationId::generate())
            .await
            .unwrap();
        assert_eq!(action.working_directory, None);
        assert!(!action.do_not_cache);
    }

    fn platform(pairs: &[(&str, &str)]) -> Platform {
        use crate::proto::build::bazel::remote::execution::v2::platform::Property;
        Platform {
            properties: pairs
                .iter()
                .map(|(name, value)| Property {
                    name: name.to_string(),
                    value: value.to_string(),
                })
                .collect(),
        }
    }

    async fn requirements_for(
        f: &Fixture,
        action_platform: Option<Platform>,
        command_platform: Option<Platform>,
    ) -> Result<WorkerRequirements, Status> {
        #[allow(deprecated)]
        let command = Command {
            platform: command_platform,
            ..Default::default()
        };
        let digest = store_action_on(&f.cas, command, false, action_platform).await;
        f.service
            .create_executable_action(execute_request(digest), None, OperationId::generate())
            .await
            .map(|action| action.requirements)
    }

    #[tokio::test]
    async fn the_action_platform_becomes_the_worker_requirements() {
        let f = fixture().await;
        let requirements = requirements_for(
            &f,
            Some(platform(&[("OSFamily", "linux"), ("ISA", "arm-a64")])),
            Some(platform(&[("OSFamily", "windows")])),
        )
        .await
        .unwrap();
        assert_eq!(requirements.labels, vec!["os=linux", "arch=arm64"]);
    }

    #[tokio::test]
    async fn the_command_platform_applies_when_the_action_has_none() {
        let f = fixture().await;
        let requirements = requirements_for(&f, None, Some(platform(&[("OSFamily", "linux")])))
            .await
            .unwrap();
        assert_eq!(requirements.labels, vec!["os=linux"]);
    }

    #[tokio::test]
    async fn priority_critical_queues_first_and_pool_becomes_a_requirement() {
        let f = fixture().await;
        let digest = store_action_on(
            &f.cas,
            Command::default(),
            false,
            Some(platform(&[("Pool", "fast"), ("Priority", "critical")])),
        )
        .await;
        let action = f
            .service
            .create_executable_action(execute_request(digest), None, OperationId::generate())
            .await
            .unwrap();
        assert_eq!(action.priority, QueuePriority::Critical);
        assert_eq!(action.requirements.labels, vec!["pool=fast"]);
    }

    #[tokio::test]
    async fn a_critical_action_by_metadata_queues_first_on_its_pool() {
        let mut f = fixture().await;
        f.service = f.service.with_critical_actions(
            CriticalActions::parse("Webpack://web:dist", Some("fast")).unwrap(),
        );
        let digest = store_action_on(
            &f.cas,
            Command::default(),
            false,
            Some(platform(&[("OSFamily", "linux")])),
        )
        .await;
        let metadata = RequestMetadata {
            action_mnemonic: "Webpack".into(),
            target_id: "//web:dist".into(),
            ..Default::default()
        };
        let action = f
            .service
            .create_executable_action(
                execute_request(digest),
                Some(&metadata),
                OperationId::generate(),
            )
            .await
            .unwrap();
        assert_eq!(action.priority, QueuePriority::Critical);
        assert_eq!(action.requirements.labels, vec!["os=linux", "pool=fast"]);
    }

    #[tokio::test]
    async fn the_remote_apis_testing_platform_needs_chroot_as_an_extra_property() {
        let mut f = fixture().await;
        let matrix = || Some(platform(&[("OSFamily", "linux"), ("CHROOT", "false")]));
        let error = requirements_for(&f, matrix(), None).await.unwrap_err();
        assert_eq!(error.code(), tonic::Code::InvalidArgument);

        f.service = f
            .service
            .with_extra_platform_properties(parse_property_names(" CHROOT, ,"));
        let requirements = requirements_for(&f, matrix(), None).await.unwrap();
        assert_eq!(requirements.labels, vec!["os=linux", "CHROOT=false"]);
    }

    #[tokio::test]
    async fn the_execution_policy_priority_reaches_the_action_within_the_advertised_range() {
        use crate::proto::build::bazel::remote::execution::v2::ExecutionPolicy;
        let f = fixture().await;
        let digest = store_action(&f.cas, Command::default(), false).await;
        for (sent, kept) in [
            (None, 0),
            (Some(250), 250),
            (Some(5000), 1000),
            (Some(-5), 0),
        ] {
            let request = ExecuteRequest {
                execution_policy: sent.map(|priority| ExecutionPolicy { priority }),
                ..execute_request(digest.clone())
            };
            let action = f
                .service
                .create_executable_action(request, None, OperationId::generate())
                .await
                .unwrap();
            assert_eq!(action.execution_priority, kept, "{sent:?}");
        }
    }

    #[tokio::test]
    async fn an_unsupported_platform_is_invalid_argument() {
        let f = fixture().await;
        let error = requirements_for(&f, Some(platform(&[("container-image", "x")])), None)
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn phase_durations_cover_the_phases_that_ran_and_skip_the_rest() {
        let mut result = worker_result(0, None);
        result.timestamps = PhaseTimestamps {
            worker_start_ms: 1_000_000_500,
            execution_start_ms: 1_000_000_600,
            execution_completed_ms: 1_000_002_600,
            output_upload_start_ms: 1_000_002_700,
            output_upload_completed_ms: 1_000_002_650,
            ..Default::default()
        };
        let queued = UNIX_EPOCH + Duration::from_millis(1_000_000_000);
        assert_eq!(
            phase_durations(&result, Some(queued)),
            vec![
                ("queued", Duration::from_millis(500)),
                ("execution", Duration::from_secs(2)),
            ]
        );
        assert_eq!(phase_durations(&result, None).len(), 1);
    }

    #[test]
    fn the_worker_phases_and_queue_time_become_the_execution_metadata() {
        let mut result = worker_result(0, None);
        result.worker_id = "rbe-worker-fast-0".into();
        result.timestamps = PhaseTimestamps {
            worker_start_ms: 1_000_001_500,
            execution_start_ms: 1_000_002_000,
            execution_completed_ms: 1_000_003_250,
            ..Default::default()
        };
        let queued = UNIX_EPOCH + Duration::from_millis(1_000_000_000);
        let metadata = execution_metadata(&result, Some(queued));
        assert_eq!(metadata.worker, "rbe-worker-fast-0");
        assert_eq!(metadata.queued_timestamp.unwrap().seconds, 1_000_000);
        let completed = metadata.execution_completed_timestamp.unwrap();
        assert_eq!(
            (completed.seconds, completed.nanos),
            (1_000_003, 250_000_000)
        );
        assert!(metadata.input_fetch_start_timestamp.is_none());
    }

    #[tokio::test]
    async fn a_nonzero_exit_is_an_ok_status_with_the_exit_code_in_the_result() {
        let f = fixture().await;
        let response = recorder(&f).response(&worker_result(3, None), None).await;
        assert_eq!(response.status.unwrap().code, 0);
        assert_eq!(response.result.unwrap().exit_code, 3);
    }

    #[tokio::test]
    async fn an_action_that_could_not_run_is_an_internal_status() {
        let f = fixture().await;
        let response = recorder(&f)
            .response(&worker_result(-1, Some("inputs missing")), None)
            .await;
        let status = response.status.unwrap();
        assert_eq!(status.code, RPC_INTERNAL);
        assert_eq!(status.message, "inputs missing");
    }

    #[tokio::test]
    async fn a_timed_out_action_is_deadline_exceeded_with_its_partial_result() {
        let f = fixture().await;
        let mut result = worker_result(-1, None);
        result.timed_out = true;
        let response = recorder(&f).response(&result, None).await;
        assert_eq!(response.status.unwrap().code, RPC_DEADLINE_EXCEEDED);
        assert!(response.result.is_some());
    }

    fn recorder(f: &Fixture) -> ActionRecorder {
        ActionRecorder::new(f.action_cache.clone(), f.cas.clone())
    }

    #[tokio::test]
    async fn the_recorder_caches_a_success_without_any_client_and_respects_do_not_cache() {
        let f = fixture().await;
        for do_not_cache in [false, true] {
            let digest = store_action(&f.cas, Command::default(), do_not_cache).await;
            let action = f
                .service
                .create_executable_action(execute_request(digest), None, OperationId::generate())
                .await
                .unwrap();
            let sm = ExecutionStateMachine::new(action.operation_id, action.action_digest);

            let response = recorder(&f)
                .record(Some(&action), &sm, &worker_result(0, None))
                .await;

            assert_eq!(response.result.unwrap().exit_code, 0);
            assert_eq!(
                ActionCacheStore::get(&*f.action_cache, &action.action_digest)
                    .await
                    .is_some(),
                !do_not_cache,
                "do_not_cache={do_not_cache}"
            );
        }
    }

    async fn cached_hit(f: &Fixture, output: &[u8], store_output: bool) -> Digest {
        use crate::cache::action_cache::CacheActionResult;
        use crate::proto::build::bazel::remote::execution::v2::OutputFile;
        let action = store_action(&f.cas, Command::default(), false).await;
        let (output_info, output_digest) = digest_of(output);
        if store_output {
            f.cas
                .write(&output_info, Bytes::from(output.to_vec()))
                .await
                .unwrap();
        }
        let action_info = DigestInfo::new(&action.hash, action.size_bytes).unwrap();
        let proto = ActionResult {
            output_files: vec![OutputFile {
                path: "out.txt".to_string(),
                digest: Some(output_digest),
                ..Default::default()
            }],
            ..Default::default()
        };
        f.action_cache
            .put(action_info, CacheActionResult::new(action_info, proto));
        action
    }

    async fn first_operation(f: &Fixture, action: Digest) -> Operation {
        let mut stream = f
            .service
            .execute(Request::new(execute_request(action)))
            .await
            .unwrap()
            .into_inner();
        stream.next().await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn execute_serves_a_cache_hit_whose_outputs_are_in_the_cas() {
        let f = fixture().await;
        let action = cached_hit(&f, b"present output", true).await;
        let op = first_operation(&f, action).await;
        assert!(op.done, "a verified hit completes the operation at once");
    }

    #[tokio::test]
    async fn a_cache_hit_served_by_execute_counts_for_its_target() {
        use crate::server::request_metadata::HEADER;
        use prost::Message;
        use tonic::metadata::MetadataValue;

        let f = fixture().await;
        let action = cached_hit(&f, b"logged output", true).await;
        let log = Arc::new(ActionCacheLog::new());
        let service = f.service.with_action_cache_log(log.clone());
        let caller = RequestMetadata {
            tool_invocation_id: "inv-1".into(),
            target_id: "//pkg:target".into(),
            action_mnemonic: "GoCompilePkg".into(),
            ..Default::default()
        };
        let mut request = Request::new(execute_request(action));
        request
            .metadata_mut()
            .insert_bin(HEADER, MetadataValue::from_bytes(&caller.encode_to_vec()));

        let op = service
            .execute(request)
            .await
            .unwrap()
            .into_inner()
            .next()
            .await
            .unwrap()
            .unwrap();

        assert!(op.done);
        let stats = log.snapshot("inv-1").await;
        assert_eq!(stats["//pkg:target"].hits, 1);
        assert_eq!(stats["//pkg:target"].misses, 0);
    }

    #[tokio::test]
    async fn execute_drops_a_cache_hit_whose_outputs_were_evicted() {
        let f = fixture().await;
        let action = cached_hit(&f, b"evicted output", false).await;
        let action_info = DigestInfo::new(&action.hash, action.size_bytes).unwrap();
        let op = first_operation(&f, action).await;
        assert!(!op.done, "the action is queued for execution, not served");
        assert!(f.action_cache.get(&action_info).is_none());
    }

    #[tokio::test]
    async fn only_a_clean_success_is_written_to_the_action_cache() {
        let f = fixture().await;
        let cases = [
            ("success", worker_result(0, None), true),
            ("nonzero", worker_result(1, None), false),
            ("error", worker_result(-1, Some("spawn failed")), false),
        ];
        for (name, result, cached) in cases {
            let (digest, _) = digest_of(name.as_bytes());
            let response = recorder(&f).response(&result, None).await;
            cache_success(&*f.action_cache, digest, &result, &response).await;
            assert_eq!(
                ActionCacheStore::get(&*f.action_cache, &digest)
                    .await
                    .is_some(),
                cached,
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn an_identical_action_in_flight_is_joined_instead_of_run_twice() {
        let f = fixture().await;
        let digest = store_action(&f.cas, Command::default(), false).await;

        let mut names = Vec::new();
        for _ in 0..2 {
            let mut stream = f
                .service
                .execute(Request::new(execute_request(digest.clone())))
                .await
                .unwrap()
                .into_inner();
            let first = stream.next().await.unwrap().unwrap();
            assert!(!first.done);
            names.push(first.name);
        }

        assert_eq!(
            names[0], names[1],
            "the second Execute follows the first operation"
        );
        assert!(
            f.service.scheduler.dequeue().is_some(),
            "the action is queued"
        );
        assert!(
            f.service.scheduler.dequeue().is_none(),
            "and queued only once"
        );
    }
}
