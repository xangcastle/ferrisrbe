#[allow(unused_imports)]
use bytes::Bytes;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use crate::cache::action_cache::L1ActionCache;
#[allow(unused_imports)]
use crate::cas::CasBackend;
use crate::cas::SharedCasBackend;
use crate::execution::output_handler::OutputHandler;
use crate::execution::results::ResultsStore;
use crate::execution::scheduler::{ExecutableAction, MultiLevelScheduler, QueuePriority};
use crate::execution::state_machine::StateMachineManager;
use crate::proto::build::bazel::remote::execution::v2::{
    execution_server::{Execution, ExecutionServer},
    Action, ActionResult, Command, Digest, ExecuteRequest, ExecuteResponse, OutputDirectory,
    OutputFile, WaitExecutionRequest,
};
use crate::proto::google::longrunning::{operation::Result as OpResult, Operation};
use crate::proto::google::rpc::Status as RpcStatus;
use crate::types::DigestInfo;

use prost::Message as ProstMessage;
use prost_types::Any;
use std::sync::Arc;
use std::time::Duration;
use tracing::debug;

pub struct ExecutionService {
    scheduler: Arc<MultiLevelScheduler>,
    state_manager: Arc<StateMachineManager>,
    l1_cache: Arc<L1ActionCache>,
    results_store: Arc<ResultsStore>,
    output_handler: OutputHandler,
    cas_backend: SharedCasBackend,
}

impl ExecutionService {
    pub fn new(
        scheduler: Arc<MultiLevelScheduler>,
        state_manager: Arc<StateMachineManager>,
        l1_cache: Arc<L1ActionCache>,
        results_store: Arc<ResultsStore>,
        cas_backend: SharedCasBackend,
    ) -> Self {
        Self {
            scheduler,
            state_manager,
            l1_cache,
            results_store,
            output_handler: OutputHandler::new(cas_backend.clone()),
            cas_backend,
        }
    }

    pub fn into_service(self) -> ExecutionServer<Self> {
        let max_msg_size = std::env::var("RBE_MAX_GRPC_MSG_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100 * 1024 * 1024);
        ExecutionServer::new(self)
            .max_decoding_message_size(max_msg_size)
            .max_encoding_message_size(max_msg_size)
    }

    async fn create_executable_action(
        &self,
        req: ExecuteRequest,
        operation_id: crate::execution::state_machine::OperationId,
    ) -> Result<ExecutableAction, Status> {
        let action_digest = req
            .action_digest
            .clone()
            .map(|d| DigestInfo::new(&d.hash, d.size_bytes))
            .unwrap_or_else(|| DigestInfo::new("unknown", 0));

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
        let command_digest_info = DigestInfo::new(&command_digest.hash, command_digest.size_bytes);

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
            .map(|d| DigestInfo::new(&d.hash, d.size_bytes));

        let (output_files, output_directories) = if !command.output_paths.is_empty() {
            (command.output_paths.clone(), vec![])
        } else {
            #[allow(deprecated)]
            (command.output_files, command.output_directories)
        };

        Ok(ExecutableAction {
            operation_id,
            action_digest,
            input_digests: Vec::new(),
            command: command.arguments,
            // REAPI v2.4: timeout is Option<prost_types::Duration>
            timeout: action
                .timeout
                .and_then(|d| {
                    if d.seconds > 0 {
                        Some(Duration::from_secs(d.seconds as u64))
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| Duration::from_secs(300)),
            priority: QueuePriority::Medium,
            output_files,
            output_directories,
            working_directory: None,
            input_root_digest,
        })
    }

    async fn fetch_action(&self, digest: &DigestInfo) -> crate::cas::CasResult<Option<Action>> {
        let data = self.cas_backend.read(digest).await?;
        match data {
            Some(bytes) => {
                let action = Action::decode(bytes).map_err(|e| {
                    crate::cas::CasError::InvalidData(format!("Failed to decode Action: {}", e))
                })?;
                Ok(Some(action))
            }
            None => Ok(None),
        }
    }

    async fn fetch_command(&self, digest: &DigestInfo) -> crate::cas::CasResult<Option<Command>> {
        let data = self.cas_backend.read(digest).await?;
        match data {
            Some(bytes) => {
                let command = Command::decode(bytes).map_err(|e| {
                    crate::cas::CasError::InvalidData(format!("Failed to decode Command: {}", e))
                })?;
                Ok(Some(command))
            }
            None => Ok(None),
        }
    }

    /// Build ExecuteResponse with output handling for large stdout/stderr
    /// This is a static method that can be called from within spawned tasks
    async fn build_execute_response_with_handler(
        result: &crate::worker::k8s::ExecutionResult,
        output_handler: &crate::execution::output_handler::OutputHandler,
    ) -> ExecuteResponse {
        let stdout_result = output_handler
            .process_output("stdout", result.stdout.clone())
            .await
            .unwrap_or_else(|e| {
                warn!("Failed to process stdout: {}, using inline", e);
                crate::execution::output_handler::OutputResult::inline(result.stdout.clone())
            });

        let stderr_result = output_handler
            .process_output("stderr", result.stderr.clone())
            .await
            .unwrap_or_else(|e| {
                warn!("Failed to process stderr: {}, using inline", e);
                crate::execution::output_handler::OutputResult::inline(result.stderr.clone())
            });

        debug!(
            "Output handling: stdout_inline={}, stdout_stored={}, stderr_inline={}, stderr_stored={}",
            stdout_result.is_inline(),
            stdout_result.is_stored(),
            stderr_result.is_inline(),
            stderr_result.is_stored()
        );

        let output_files: Vec<OutputFile> = result
            .output_files
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
            .collect();

        let output_directories: Vec<OutputDirectory> = result
            .output_directories
            .iter()
            .map(|d| OutputDirectory {
                path: d.path.clone(),
                tree_digest: Some(Digest {
                    hash: d.digest.clone(),
                    size_bytes: d.size_bytes,
                }),
                // REAPI v2.4: Additional fields
                is_topologically_sorted: false,
                root_directory_digest: None,
            })
            .collect();

        let stdout_raw = if stdout_result.is_inline() {
            stdout_result.raw.unwrap_or_default()
        } else {
            Vec::new()
        };

        let stdout_digest = stdout_result.digest.map(|d| Digest {
            hash: d.hash_to_string(),
            size_bytes: d.size,
        });

        let stderr_raw = if stderr_result.is_inline() {
            stderr_result.raw.unwrap_or_default()
        } else {
            Vec::new()
        };

        let stderr_digest = stderr_result.digest.map(|d| Digest {
            hash: d.hash_to_string(),
            size_bytes: d.size,
        });

        // REAPI v2.4: ActionResult no longer has exit_details
        #[allow(deprecated)]
        let action_result = ActionResult {
            output_files,
            output_directories,
            exit_code: result.exit_code,
            stdout_raw,
            stdout_digest,
            stderr_raw,
            stderr_digest,
            execution_metadata: None,
            // Additional v2.4 fields with default values
            output_file_symlinks: vec![],
            output_symlinks: vec![],
            output_directory_symlinks: vec![],
        };

        // REAPI v2.4: ExecuteResponse requires server_logs
        ExecuteResponse {
            result: Some(action_result),
            cached_result: false,
            status: Some(RpcStatus {
                code: if result.exit_code == 0 { 0 } else { 1 },
                message: if result.exit_code == 0 {
                    "OK".to_string()
                } else {
                    format!("Exit code: {}", result.exit_code)
                },
                details: vec![],
            }),
            message: "Execution completed".to_string(),
            server_logs: std::collections::HashMap::new(),
        }
    }
}

#[tonic::async_trait]
impl Execution for ExecutionService {
    type ExecuteStream = ReceiverStream<Result<Operation, Status>>;

    async fn execute(
        &self,
        request: Request<ExecuteRequest>,
    ) -> Result<Response<ReceiverStream<Result<Operation, Status>>>, Status> {
        let req = request.into_inner();
        let action_digest = req
            .action_digest
            .clone()
            .ok_or_else(|| Status::invalid_argument("Missing action_digest"))?;

        let digest_info = DigestInfo::new(&action_digest.hash, action_digest.size_bytes);

        info!("Execution::Execute digest={}", digest_info);

        let (tx, rx) = tokio::sync::mpsc::channel(10);

        if !req.skip_cache_lookup {
            if let Some(cached_result) = self.l1_cache.get(&digest_info) {
                info!("Cache hit for action {}", digest_info);

                #[allow(deprecated)]
                let exec_response = ExecuteResponse {
                    result: Some(ActionResult {
                        output_files: cached_result
                            .output_files
                            .iter()
                            .map(|f| OutputFile {
                                path: f.path.clone(),
                                digest: Some(Digest {
                                    hash: f.digest.hash_to_string(),
                                    size_bytes: f.digest.size,
                                }),
                                is_executable: f.is_executable,
                                contents: vec![],
                                node_properties: None,
                            })
                            .collect(),
                        output_directories: cached_result
                            .output_directories
                            .iter()
                            .map(|d| {
                                let digest = &d.tree_digest;
                                OutputDirectory {
                                    path: d.path.clone(),
                                    tree_digest: Some(Digest {
                                        hash: digest.hash_to_string(),
                                        size_bytes: digest.size,
                                    }),
                                    is_topologically_sorted: false,
                                    root_directory_digest: None,
                                }
                            })
                            .collect(),
                        exit_code: cached_result.exit_code,
                        stdout_raw: vec![],
                        stdout_digest: cached_result.stdout_digest.map(|d| Digest {
                            hash: d.hash_to_string(),
                            size_bytes: d.size,
                        }),
                        stderr_raw: vec![],
                        stderr_digest: cached_result.stderr_digest.map(|d| Digest {
                            hash: d.hash_to_string(),
                            size_bytes: d.size,
                        }),
                        execution_metadata: None,
                        output_file_symlinks: vec![],
                        output_symlinks: vec![],
                        output_directory_symlinks: vec![],
                    }),
                    cached_result: true,
                    status: Some(RpcStatus {
                        code: if cached_result.exit_code == 0 { 0 } else { 1 },
                        message: if cached_result.exit_code == 0 {
                            "OK (cached)".to_string()
                        } else {
                            format!("Exit code: {} (cached)", cached_result.exit_code)
                        },
                        details: vec![],
                    }),
                    message: "Execution result served from cache".to_string(),
                    server_logs: std::collections::HashMap::new(),
                };

                let op_result = OpResult::Response({
                    let mut buf = Vec::new();
                    ProstMessage::encode(&exec_response, &mut buf).expect("encode failed");
                    Any {
                        type_url:
                            "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse"
                                .to_string(),
                        value: buf,
                    }
                });

                let op = Operation {
                    name: format!("operations/{}", digest_info),
                    metadata: None,
                    done: true,
                    result: Some(op_result),
                };

                let _ = tx.send(Ok(op)).await;
                return Ok(Response::new(ReceiverStream::new(rx)));
            }
        }

        let operation_id = crate::execution::state_machine::OperationId::generate();
        let sm = self
            .state_manager
            .create_machine_with_id(operation_id, digest_info);
        let action = match self.create_executable_action(req, operation_id).await {
            Ok(action) => action,
            Err(e) => {
                let _ = tx.send(Err(e)).await;
                return Ok(Response::new(ReceiverStream::new(rx)));
            }
        };
        let results_store = self.results_store.clone();

        match self.scheduler.enqueue(action, sm.clone()) {
            Ok(_) => {
                info!("Enqueued operation {}", operation_id);

                let op = Operation {
                    name: format!("operations/{}", operation_id.0),
                    metadata: None,
                    done: false,
                    result: None,
                };
                let _ = tx.send(Ok(op)).await;

                let tx_clone = tx.clone();
                let sm_clone = sm.clone();
                let output_handler = self.output_handler.clone();

                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        let done = sm_clone.is_terminal().await;

                        if done {
                            let result = results_store.get(operation_id);

                            let op_result = if let Some(r) = result {
                                let exec_response =
                                    Self::build_execute_response_with_handler(&r, &output_handler)
                                        .await;

                                OpResult::Response({
                                    let mut buf = Vec::new();
                                    ProstMessage::encode(&exec_response, &mut buf)
                                        .expect("encode failed");
                                    Any {
                                        type_url: "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse".to_string(),
                                        value: buf,
                                    }
                                })
                            } else {
                                warn!("No result found for operation {}", operation_id.0);
                                OpResult::Response({
                                    // REAPI v2.4: ExecuteResponse requires server_logs
                                    let exec_response = ExecuteResponse {
                                        result: None,
                                        cached_result: false,
                                        status: Some(RpcStatus {
                                            code: 2,
                                            message: "Execution result not found".to_string(),
                                            details: vec![],
                                        }),
                                        message: "Execution result not found".to_string(),
                                        server_logs: std::collections::HashMap::new(),
                                    };
                                    let mut buf = Vec::new();
                                    ProstMessage::encode(&exec_response, &mut buf)
                                        .expect("encode failed");
                                    Any {
                                        type_url: "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse".to_string(),
                                        value: buf,
                                    }
                                })
                            };

                            let op = Operation {
                                name: format!("operations/{}", operation_id.0),
                                metadata: None,
                                done: true,
                                result: Some(op_result),
                            };

                            let _ = tx_clone.send(Ok(op)).await;
                            break;
                        }

                        let op = Operation {
                            name: format!("operations/{}", operation_id.0),
                            metadata: None,
                            done: false,
                            result: None,
                        };

                        if tx_clone.send(Ok(op)).await.is_err() {
                            break;
                        }
                    }
                });
            }
            Err(e) => {
                warn!("Failed to enqueue action: {}", e);
                let _ = tx
                    .send(Err(Status::internal("Failed to enqueue action")))
                    .await;
            }
        }

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    type WaitExecutionStream = ReceiverStream<Result<Operation, Status>>;

    async fn wait_execution(
        &self,
        request: Request<WaitExecutionRequest>,
    ) -> Result<Response<ReceiverStream<Result<Operation, Status>>>, Status> {
        let req = request.into_inner();
        info!("Execution::WaitExecution name={}", req.name);

        let (tx, rx) = tokio::sync::mpsc::channel(10);

        let op_id_str = req.name.trim_start_matches("operations/");
        let op_id: u64 = op_id_str
            .parse()
            .map_err(|_| Status::invalid_argument("Invalid operation name"))?;

        let op_id = crate::execution::state_machine::OperationId(op_id);
        let results_store = self.results_store.clone();

        if let Some(sm) = self.state_manager.get_machine(op_id) {
            let tx_clone = tx.clone();

            tokio::spawn(async move {
                loop {
                    let done = sm.is_terminal().await;

                    if done {
                        let result = results_store.get(op_id);

                        let op_result = result.map(|r| {
                            // REAPI v2.4: ActionResult without exit_details, ExecuteResponse with server_logs
                            #[allow(deprecated)]
                            let exec_response = ExecuteResponse {
                                result: Some(ActionResult {
                                    // REAPI v2.4: OutputFile has specific fields
                                    output_files: r.output_files.iter().map(|f| OutputFile {
                                        path: f.path.clone(),
                                        digest: Some(Digest {
                                            hash: f.digest.clone(),
                                            size_bytes: f.size_bytes,
                                        }),
                                        is_executable: f.is_executable,
                                        contents: vec![],
                                        node_properties: None,
                                    }).collect(),
                                    output_directories: r.output_directories.iter().map(|d| OutputDirectory {
                                        path: d.path.clone(),
                                        tree_digest: Some(Digest {
                                            hash: d.digest.clone(),
                                            size_bytes: d.size_bytes,
                                        }),
                                        is_topologically_sorted: false,
                                        root_directory_digest: None,
                                    }).collect(),
                                    exit_code: r.exit_code,
                                    // REAPI v2.4: Without exit_details
                                    stdout_raw: r.stdout.clone(),
                                    stdout_digest: r.stdout_digest.as_ref().map(|h| Digest {
                                        hash: h.clone(),
                                        size_bytes: r.stdout.len() as i64,
                                    }),
                                    stderr_raw: r.stderr.clone(),
                                    stderr_digest: r.stderr_digest.as_ref().map(|h| Digest {
                                        hash: h.clone(),
                                        size_bytes: r.stderr.len() as i64,
                                    }),
                                    execution_metadata: None,
                                    // REAPI v2.4: Additional fields
                                    output_file_symlinks: vec![],
                                    output_symlinks: vec![],
                                    output_directory_symlinks: vec![],
                                }),
                                cached_result: false,
                                status: Some(RpcStatus {
                                    code: if r.exit_code == 0 { 0 } else { 1 },
                                    message: if r.exit_code == 0 { "OK".to_string() } else { format!("Exit code: {}", r.exit_code) },
                                    details: vec![],
                                }),
                                message: "Execution completed".to_string(),
                                server_logs: std::collections::HashMap::new(),
                            };

                            OpResult::Response({
                                let mut buf = Vec::new();
                                ProstMessage::encode(&exec_response, &mut buf).expect("encode failed");
                                Any {
                                    type_url: "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse".to_string(),
                                    value: buf,
                                }
                            })
                        });

                        let op = Operation {
                            name: req.name.clone(),
                            metadata: None,
                            done: true,
                            result: op_result,
                        };

                        let _ = tx_clone.send(Ok(op)).await;
                        break;
                    }

                    let op = Operation {
                        name: req.name.clone(),
                        metadata: None,
                        done: false,
                        result: None,
                    };

                    if tx_clone.send(Ok(op)).await.is_err() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            });
        } else {
            let _ = tx.send(Err(Status::not_found("Operation not found"))).await;
        }

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}
