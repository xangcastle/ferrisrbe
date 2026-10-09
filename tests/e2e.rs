//! End-to-end remote execution: the real `server` and `worker` binaries,
//! driven over REAPI the way Bazel drives them.
//!
//! Every test starts its own server and worker on free local ports, uploads
//! an `Action`, its `Command` and input root to the CAS, calls `Execute`, and
//! checks the `ExecuteResponse` and, through the CAS and the Action Cache,
//! what the worker produced.

use futures::StreamExt;
use prost::Message;
use rbe_server::grpc_credentials::BearerAuth;
use rbe_server::proto::build::bazel::remote::execution::v2::{
    action_cache_client::ActionCacheClient, batch_update_blobs_request,
    content_addressable_storage_client::ContentAddressableStorageClient,
    execution_client::ExecutionClient, platform, Action, ActionResult, BatchReadBlobsRequest,
    BatchUpdateBlobsRequest, Command, Digest, Directory, DirectoryNode, ExecuteRequest,
    ExecuteResponse, FileNode, GetActionResultRequest, Platform, SymlinkNode, WaitExecutionRequest,
};
use rbe_server::proto::google::bytestream::{byte_stream_client::ByteStreamClient, WriteRequest};
use rbe_server::proto::google::longrunning::Operation;
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command as Process};
use tonic::transport::Channel;
use tonic::Code;

/// How long a test waits for an operation before giving up.
const OPERATION_DEADLINE: Duration = Duration::from_secs(60);

/// A server and a worker connected to it, killed when dropped.
struct Cluster {
    dir: tempfile::TempDir,
    _server: Child,
    worker: Child,
    channel: Channel,
    /// What the test client presents; the worker is given the same token.
    auth: BearerAuth,
}

/// A port no other cluster of this test process was given. The OS can hand
/// a just-released port out again, and two clusters sharing one would bind
/// the same address, or connect a test to another test's server.
fn free_port() -> u16 {
    static GIVEN: Mutex<BTreeSet<u16>> = Mutex::new(BTreeSet::new());
    loop {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        if GIVEN.lock().unwrap().insert(port) {
            return port;
        }
    }
}

fn binary(env: &str) -> PathBuf {
    PathBuf::from(std::env::var(env).unwrap_or_else(|_| panic!("{env} is not set")))
}

fn log_file(dir: &tempfile::TempDir, name: &str) -> Stdio {
    std::fs::File::create(dir.path().join(name)).unwrap().into()
}

/// Server starts attempted before a test gives up; a start fails when
/// another process takes one of its ports between choosing and binding it.
const SERVER_START_ATTEMPTS: usize = 5;

/// How long a server that accepts connections must stay alive before it is
/// trusted to own its ports.
const SERVER_SETTLE: Duration = Duration::from_millis(300);

/// Starts a server on fresh ports, and returns it with its endpoint and a
/// connected channel once both of its ports accept connections and it is
/// still running [`SERVER_SETTLE`] later. A server that exits meanwhile (its
/// port was taken, typically) is an `Err` with its stderr, so the caller can
/// retry on other ports; checking liveness also rules out having connected to
/// another process listening on the same port.
async fn start_server(
    dir: &tempfile::TempDir,
    token: Option<&str>,
) -> Result<(Child, String, Channel), String> {
    let grpc_port = free_port();
    let http_port = free_port();
    let endpoint = format!("http://127.0.0.1:{grpc_port}");

    let mut server = Process::new(binary("RBE_SERVER_BIN"));
    if let Some(token) = token {
        server
            .env("RBE_GRPC_AUTH_ENABLED", "true")
            .env("RBE_AUTH_STATIC_TOKENS", token);
    }
    let mut server = server
        .env("RBE_BIND_ADDRESS", "127.0.0.1")
        .env("RBE_PORT", grpc_port.to_string())
        .env("RBE_HTTP_PORT", http_port.to_string())
        .env("RBE_CACHE_DIR", dir.path().join("cache"))
        .env("RBE_BES_DATA_DIR", dir.path().join("bes"))
        .env("RUST_LOG", "info")
        .stdout(log_file(dir, "server.log"))
        .stderr(log_file(dir, "server.err"))
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let exited = |server: &mut Child| -> Option<String> {
        server.try_wait().unwrap().map(|status| {
            let stderr = std::fs::read_to_string(dir.path().join("server.err")).unwrap_or_default();
            format!("server on {endpoint} exited with {status}:\n{stderr}")
        })
    };

    let deadline = Instant::now() + Duration::from_secs(30);
    let channel = loop {
        if let Some(failure) = exited(&mut server) {
            return Err(failure);
        }
        let http_up = tokio::net::TcpStream::connect(("127.0.0.1", http_port))
            .await
            .is_ok();
        let grpc = Channel::from_shared(endpoint.clone())
            .unwrap()
            .connect()
            .await;
        match grpc {
            Ok(channel) if http_up => break channel,
            _ if Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(50)).await,
            _ => return Err(format!("server never listened on {endpoint}")),
        }
    };

    tokio::time::sleep(SERVER_SETTLE).await;
    match exited(&mut server) {
        Some(failure) => Err(failure),
        None => Ok((server, endpoint, channel)),
    }
}

impl Cluster {
    async fn start() -> Self {
        Self::start_configured(None, None, None).await
    }

    /// A cluster whose server requires `token` on every gRPC call, as
    /// production does, when one is given.
    async fn start_with_token(token: Option<&str>) -> Self {
        Self::start_configured(token, None, None).await
    }

    /// A cluster whose worker runs up to `max_concurrent` actions at once.
    async fn start_with_capacity(max_concurrent: usize) -> Self {
        Self::start_configured(None, Some(max_concurrent), None).await
    }

    /// A cluster whose worker advertises exactly `labels` (`WORKER_LABELS`).
    async fn start_with_labels(labels: &str) -> Self {
        Self::start_configured(None, None, Some(labels)).await
    }

    async fn start_configured(
        token: Option<&str>,
        max_concurrent: Option<usize>,
        labels: Option<&str>,
    ) -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let mut failures = Vec::new();
        let (server, endpoint, channel) = loop {
            match start_server(&dir, token).await {
                Ok(started) => break started,
                Err(failure) if failures.len() + 1 < SERVER_START_ATTEMPTS => {
                    failures.push(failure)
                }
                Err(failure) => {
                    failures.push(failure);
                    panic!("the server did not start:\n{}", failures.join("\n---\n"));
                }
            }
        };

        let mut worker = Process::new(binary("RBE_WORKER_BIN"));
        if let Some(token) = token {
            worker.env("RBE_WORKER_TOKEN", token);
        }
        if let Some(max_concurrent) = max_concurrent {
            worker.env("MAX_CONCURRENT", max_concurrent.to_string());
        }
        if let Some(labels) = labels {
            worker.env("WORKER_LABELS", labels);
        }
        let worker = worker
            .env("SERVER_ENDPOINT", &endpoint)
            .env("CAS_ENDPOINT", &endpoint)
            .env("WORKDIR", dir.path().join("work"))
            .env("WORKER_ID", "e2e-worker")
            .env("RUST_LOG", "info")
            .stdout(log_file(&dir, "worker.log"))
            .stderr(log_file(&dir, "worker.err"))
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        Self {
            dir,
            _server: server,
            worker,
            channel,
            auth: BearerAuth::new(token).unwrap(),
        }
    }

    /// Waits until the worker's log contains `needle`, so a test acts on a
    /// state the worker reached rather than on a guess of how long it takes.
    async fn wait_for_worker_log(&self, needle: &str) {
        let deadline = Instant::now() + OPERATION_DEADLINE;
        while Instant::now() < deadline {
            let log =
                std::fs::read_to_string(self.dir.path().join("worker.log")).unwrap_or_default();
            if log.contains(needle) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the worker never logged {needle:?}\n{}", self.logs());
    }

    /// Sends SIGTERM to the worker, as Kubernetes does to drain a pod.
    fn terminate_worker(&self) {
        let pid = self.worker.id().expect("the worker is running");
        let status = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// The server and worker logs, to explain a failed assertion.
    fn logs(&self) -> String {
        ["server.log", "server.err", "worker.log", "worker.err"]
            .iter()
            .map(|name| {
                let text = std::fs::read_to_string(self.dir.path().join(name)).unwrap_or_default();
                format!("===== {name}\n{text}")
            })
            .collect()
    }

    async fn upload(&self, blobs: Vec<Vec<u8>>) {
        let requests = blobs
            .into_iter()
            .map(|data| batch_update_blobs_request::Request {
                digest: Some(digest_of(&data)),
                data,
                compressor: 0,
            })
            .collect();
        let response = ContentAddressableStorageClient::with_interceptor(
            self.channel.clone(),
            self.auth.clone(),
        )
        .batch_update_blobs(BatchUpdateBlobsRequest {
            requests,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
        for r in response.responses {
            assert_eq!(r.status.map_or(0, |s| s.code), 0, "upload failed");
        }
    }

    /// One `ByteStream.Write` of `data` in a single request with
    /// `finish_write`, at `write_offset`, whose request stream then stays
    /// open, like a client that waits for the response before half-closing.
    async fn write_in_one_request(
        &self,
        data: &[u8],
        write_offset: i64,
    ) -> Result<i64, tonic::Status> {
        let digest = digest_of(data);
        let request = WriteRequest {
            resource_name: format!("uploads/e2e/blobs/{}/{}", digest.hash, digest.size_bytes),
            write_offset,
            finish_write: true,
            data: data.to_vec(),
        };
        let requests = futures::stream::iter([request]).chain(futures::stream::pending());
        let mut client =
            ByteStreamClient::with_interceptor(self.channel.clone(), self.auth.clone());
        tokio::time::timeout(Duration::from_secs(10), client.write(requests))
            .await
            .unwrap_or_else(|_| panic!("Write never answered\n{}", self.logs()))
            .map(|response| response.into_inner().committed_size)
    }

    async fn read(&self, digest: &Digest) -> Vec<u8> {
        let response = ContentAddressableStorageClient::with_interceptor(
            self.channel.clone(),
            self.auth.clone(),
        )
        .batch_read_blobs(BatchReadBlobsRequest {
            digests: vec![digest.clone()],
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
        response.responses.into_iter().next().unwrap().data
    }

    /// Uploads `action`'s blobs and runs it to completion.
    async fn execute(&self, action: &TestAction) -> Result<ExecuteResponse, tonic::Status> {
        let stream = self.submit(action).await?;
        self.outcome(stream).await
    }

    /// Uploads `action`'s blobs and calls `Execute`, returning its stream.
    async fn submit(
        &self,
        action: &TestAction,
    ) -> Result<tonic::Streaming<Operation>, tonic::Status> {
        self.upload(action.blobs()).await;
        Ok(
            ExecutionClient::with_interceptor(self.channel.clone(), self.auth.clone())
                .execute(ExecuteRequest {
                    action_digest: Some(action.digest()),
                    ..Default::default()
                })
                .await?
                .into_inner(),
        )
    }

    /// Attaches to the operation `name` with `WaitExecution` and waits for it.
    async fn wait(&self, name: String) -> Result<ExecuteResponse, tonic::Status> {
        let stream = ExecutionClient::with_interceptor(self.channel.clone(), self.auth.clone())
            .wait_execution(WaitExecutionRequest { name })
            .await?
            .into_inner();
        self.outcome(stream).await
    }

    /// Reads an operation stream until the operation is done, and returns its
    /// `ExecuteResponse` or error.
    async fn outcome(
        &self,
        mut stream: tonic::Streaming<Operation>,
    ) -> Result<ExecuteResponse, tonic::Status> {
        let wait = async {
            while let Some(operation) = stream.message().await? {
                if operation.done {
                    use rbe_server::proto::google::longrunning::operation::Result as OpResult;
                    return match operation.result {
                        Some(OpResult::Response(any)) => {
                            Ok(ExecuteResponse::decode(any.value.as_slice()).unwrap())
                        }
                        Some(OpResult::Error(status)) => Err(tonic::Status::new(
                            Code::from_i32(status.code),
                            status.message,
                        )),
                        None => panic!("done operation without a result"),
                    };
                }
            }
            panic!("the operation stream ended before the operation was done")
        };
        match tokio::time::timeout(OPERATION_DEADLINE, wait).await {
            Ok(result) => result,
            Err(_) => panic!("operation did not finish in time\n{}", self.logs()),
        }
    }

    async fn cached_result(&self, action: &TestAction) -> Result<ActionResult, tonic::Status> {
        ActionCacheClient::with_interceptor(self.channel.clone(), self.auth.clone())
            .get_action_result(GetActionResultRequest {
                action_digest: Some(action.digest()),
                ..Default::default()
            })
            .await
            .map(tonic::Response::into_inner)
    }

    /// The stdout of a result, inline or from the CAS.
    async fn stdout(&self, result: &ActionResult) -> Vec<u8> {
        match &result.stdout_digest {
            Some(digest) if result.stdout_raw.is_empty() && digest.size_bytes > 0 => {
                self.read(digest).await
            }
            _ => result.stdout_raw.clone(),
        }
    }
}

fn digest_of(data: &[u8]) -> Digest {
    Digest {
        hash: hex::encode(Sha256::digest(data)),
        size_bytes: data.len() as i64,
    }
}

/// An action together with the blobs it references.
struct TestAction {
    command: Command,
    input_root: Vec<u8>,
    extra_blobs: Vec<Vec<u8>>,
    timeout: Option<Duration>,
    platform: Option<Platform>,
}

impl TestAction {
    fn bash(script: &str, output_paths: &[&str]) -> Self {
        Self {
            command: Command {
                arguments: vec!["/bin/bash".into(), "-c".into(), script.into()],
                output_paths: output_paths.iter().map(|p| p.to_string()).collect(),
                ..Default::default()
            },
            input_root: Directory::default().encode_to_vec(),
            extra_blobs: vec![],
            timeout: None,
            platform: None,
        }
    }

    fn action(&self) -> Action {
        Action {
            command_digest: Some(digest_of(&self.command.encode_to_vec())),
            input_root_digest: Some(digest_of(&self.input_root)),
            timeout: self.timeout.map(|t| prost_types::Duration {
                seconds: t.as_secs() as i64,
                nanos: 0,
            }),
            platform: self.platform.clone(),
            ..Default::default()
        }
    }

    fn digest(&self) -> Digest {
        digest_of(&self.action().encode_to_vec())
    }

    fn blobs(&self) -> Vec<Vec<u8>> {
        let mut blobs = vec![
            self.action().encode_to_vec(),
            self.command.encode_to_vec(),
            self.input_root.clone(),
        ];
        blobs.extend(self.extra_blobs.iter().cloned());
        blobs
    }
}

fn platform_of(pairs: &[(&str, &str)]) -> Platform {
    Platform {
        properties: pairs
            .iter()
            .map(|(name, value)| platform::Property {
                name: name.to_string(),
                value: value.to_string(),
            })
            .collect(),
    }
}

/// The REAPI platform of the machine the test runs on.
fn host_platform() -> Platform {
    let isa = match std::env::consts::ARCH {
        "aarch64" => "arm-a64",
        "x86_64" => "x86-64",
        other => panic!("no REAPI ISA for {other}"),
    };
    platform_of(&[("OSFamily", std::env::consts::OS), ("ISA", isa)])
}

#[tokio::test]
async fn a_terminated_worker_finishes_its_running_action_then_exits() {
    let mut cluster = Cluster::start().await;
    let action = TestAction::bash("sleep 2; echo drained > out.txt", &["out.txt"]);

    let execution = cluster.execute(&action);
    let terminate = async {
        cluster.wait_for_worker_log("Executing assignment").await;
        cluster.terminate_worker();
    };
    let (response, ()) = tokio::join!(execution, terminate);

    let result = response.unwrap().result.unwrap();
    assert_eq!(result.exit_code, 0, "{}", cluster.logs());
    assert_eq!(result.output_files.len(), 1, "{}", cluster.logs());
    let exit = tokio::time::timeout(Duration::from_secs(10), cluster.worker.wait())
        .await
        .unwrap_or_else(|_| panic!("the worker did not exit after draining\n{}", cluster.logs()))
        .unwrap();
    assert!(exit.success(), "{exit}\n{}", cluster.logs());
}

#[tokio::test]
async fn a_successful_action_returns_its_outputs_and_is_cached() {
    let cluster = Cluster::start().await;
    let action = TestAction::bash("echo hi > out.txt; echo on-stdout", &["out.txt"]);

    let response = cluster.execute(&action).await.unwrap();
    assert_eq!(response.status.unwrap().code, 0, "{}", cluster.logs());
    let result = response.result.unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(cluster.stdout(&result).await, b"on-stdout\n");
    let metadata = result
        .execution_metadata
        .clone()
        .expect("execution metadata");
    assert!(!metadata.worker.is_empty());
    let seconds = |t: &Option<prost_types::Timestamp>| t.as_ref().expect("timestamp").seconds;
    assert!(seconds(&metadata.queued_timestamp) <= seconds(&metadata.worker_start_timestamp));
    assert!(
        seconds(&metadata.execution_start_timestamp)
            <= seconds(&metadata.execution_completed_timestamp)
    );
    assert!(
        seconds(&metadata.output_upload_completed_timestamp)
            <= seconds(&metadata.worker_completed_timestamp)
    );
    assert_eq!(result.output_files.len(), 1, "{}", cluster.logs());
    let out = &result.output_files[0];
    assert_eq!(out.path, "out.txt");
    assert_eq!(out.digest.as_ref().unwrap(), &digest_of(b"hi\n"));
    assert_eq!(cluster.read(out.digest.as_ref().unwrap()).await, b"hi\n");

    let cached = cluster.cached_result(&action).await.unwrap();
    assert_eq!(cached.output_files[0].path, "out.txt");
    let again = cluster.execute(&action).await.unwrap();
    assert!(
        again.cached_result,
        "the second Execute is served from the Action Cache"
    );
}

#[tokio::test]
async fn a_failing_action_is_an_ok_status_that_keeps_its_outputs_and_is_not_cached() {
    let cluster = Cluster::start().await;
    let action = TestAction::bash("echo partial > out.txt; exit 3", &["out.txt"]);

    let response = cluster.execute(&action).await.unwrap();
    assert_eq!(response.status.unwrap().code, 0, "{}", cluster.logs());
    let result = response.result.unwrap();
    assert_eq!(result.exit_code, 3);
    assert_eq!(
        result.output_files.len(),
        1,
        "a failed test still reports its logs"
    );
    assert_eq!(
        cluster.cached_result(&action).await.unwrap_err().code(),
        Code::NotFound
    );
}

#[tokio::test]
async fn an_action_past_its_timeout_is_killed_and_deadline_exceeded() {
    let cluster = Cluster::start().await;
    let mut action = TestAction::bash("sleep 60", &[]);
    action.timeout = Some(Duration::from_secs(1));

    let started = Instant::now();
    let response = cluster.execute(&action).await.unwrap();
    assert_eq!(response.status.unwrap().code, Code::DeadlineExceeded as i32);
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn the_command_runs_in_its_working_directory_with_its_environment() {
    let cluster = Cluster::start().await;
    let pkg = Directory::default().encode_to_vec();
    let root = Directory {
        directories: vec![DirectoryNode {
            name: "pkg".into(),
            digest: Some(digest_of(&pkg)),
        }],
        ..Default::default()
    };
    let mut action = TestAction::bash(
        r#"echo "$GREETING $(basename "$PWD")" > out.txt"#,
        &["out.txt"],
    );
    action.command.working_directory = "pkg".into();
    action.command.environment_variables.push(
        rbe_server::proto::build::bazel::remote::execution::v2::command::EnvironmentVariable {
            name: "GREETING".into(),
            value: "hello".into(),
        },
    );
    action.input_root = root.encode_to_vec();
    action.extra_blobs.push(pkg);

    let response = cluster.execute(&action).await.unwrap();
    assert_eq!(response.status.unwrap().code, 0, "{}", cluster.logs());
    let result = response.result.unwrap();
    assert_eq!(result.exit_code, 0, "{}", cluster.logs());
    let out = &result.output_files[0];
    assert_eq!(
        out.path, "out.txt",
        "outputs are relative to the working directory"
    );
    assert_eq!(
        cluster.read(out.digest.as_ref().unwrap()).await,
        b"hello pkg\n"
    );
}

#[tokio::test]
async fn input_symlinks_are_materialized_and_output_symlinks_come_back_as_symlinks() {
    let cluster = Cluster::start().await;
    let content = b"through the link\n".to_vec();
    let data = Directory {
        files: vec![FileNode {
            name: "file.txt".into(),
            digest: Some(digest_of(&content)),
            ..Default::default()
        }],
        ..Default::default()
    }
    .encode_to_vec();
    let root = Directory {
        directories: vec![DirectoryNode {
            name: "data".into(),
            digest: Some(digest_of(&data)),
        }],
        symlinks: vec![SymlinkNode {
            name: "link".into(),
            target: "data/file.txt".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut action = TestAction::bash(
        "cat link > out.txt && ln -s ../not/here dangling",
        &["out.txt", "dangling"],
    );
    action.input_root = root.encode_to_vec();
    action.extra_blobs.push(data);
    action.extra_blobs.push(content.clone());

    let result = cluster.execute(&action).await.unwrap().result.unwrap();
    assert_eq!(result.exit_code, 0, "{}", cluster.logs());
    let out = &result.output_files[0];
    assert_eq!(cluster.read(out.digest.as_ref().unwrap()).await, content);
    assert_eq!(result.output_symlinks.len(), 1, "{}", cluster.logs());
    assert_eq!(result.output_symlinks[0].path, "dangling");
    assert_eq!(result.output_symlinks[0].target, "../not/here");
}

#[tokio::test]
async fn an_output_path_that_is_a_directory_is_returned_as_one() {
    let cluster = Cluster::start().await;
    let action = TestAction::bash("mkdir -p tree/sub && echo leaf > tree/sub/leaf", &["tree"]);

    let result = cluster.execute(&action).await.unwrap().result.unwrap();
    assert_eq!(result.exit_code, 0, "{}", cluster.logs());
    assert!(result.output_files.is_empty());
    assert_eq!(result.output_directories.len(), 1, "{}", cluster.logs());
    assert_eq!(result.output_directories[0].path, "tree");
}

#[tokio::test]
async fn an_action_for_the_host_platform_runs_and_an_unknown_one_is_refused() {
    let cluster = Cluster::start().await;

    let mut host = TestAction::bash("true", &[]);
    host.platform = Some(host_platform());
    let response = cluster.execute(&host).await.unwrap();
    assert_eq!(response.status.unwrap().code, 0, "{}", cluster.logs());

    let mut unknown = TestAction::bash("true", &[]);
    unknown.platform = Some(platform_of(&[("container-image", "docker://nope")]));
    assert_eq!(
        cluster.execute(&unknown).await.unwrap_err().code(),
        Code::InvalidArgument
    );
}

#[tokio::test]
async fn with_grpc_authentication_the_worker_and_the_client_present_a_token() {
    let cluster = Cluster::start_with_token(Some("e2e-token")).await;
    let action = TestAction::bash("echo authenticated > out.txt", &["out.txt"]);

    let response = cluster.execute(&action).await.unwrap();
    assert_eq!(response.status.unwrap().code, 0, "{}", cluster.logs());
    let result = response.result.unwrap();
    assert_eq!(result.exit_code, 0, "{}", cluster.logs());
    assert_eq!(
        cluster
            .read(result.output_files[0].digest.as_ref().unwrap())
            .await,
        b"authenticated\n"
    );

    let anonymous = ExecutionClient::new(cluster.channel.clone())
        .execute(ExecuteRequest {
            action_digest: Some(action.digest()),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(anonymous.code(), Code::Unauthenticated);
}

/// An action that marks itself running in `rendezvous` and succeeds only if
/// it sees `other`'s mark while it is still running, within 10 seconds.
fn meets(rendezvous: &std::path::Path, me: &str, other: &str) -> TestAction {
    let script = format!(
        "touch \"$RENDEZVOUS/{me}\"; for _ in $(seq 100); do \
         [ -e \"$RENDEZVOUS/{other}\" ] && exit 0; sleep 0.1; done; exit 1"
    );
    let mut action = TestAction::bash(&script, &[]);
    action.command.environment_variables.push(
        rbe_server::proto::build::bazel::remote::execution::v2::command::EnvironmentVariable {
            name: "RENDEZVOUS".into(),
            value: rendezvous.display().to_string(),
        },
    );
    action
}

#[tokio::test]
async fn a_critical_action_for_a_pool_runs_on_the_worker_of_that_pool() {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    let cluster = Cluster::start_with_labels(&format!(
        "os={},arch={arch},pool=fast",
        std::env::consts::OS
    ))
    .await;
    let mut action = TestAction::bash("echo fast > out.txt", &["out.txt"]);
    action.platform = Some(platform_of(&[("Pool", "fast"), ("Priority", "critical")]));

    let result = cluster.execute(&action).await.unwrap().result.unwrap();
    assert_eq!(result.exit_code, 0, "{}", cluster.logs());
}

#[tokio::test]
async fn a_burst_of_more_assignments_than_ten_all_run() {
    let cluster = Cluster::start_with_capacity(32).await;
    let actions: Vec<TestAction> = (0..32)
        .map(|i| TestAction::bash(&format!("echo {i} > out.txt"), &["out.txt"]))
        .collect();

    let responses = tokio::time::timeout(
        Duration::from_secs(60),
        futures::future::join_all(actions.iter().map(|action| cluster.execute(action))),
    )
    .await
    .unwrap_or_else(|_| panic!("the burst never finished\n{}", cluster.logs()));

    for response in responses {
        assert_eq!(
            response.unwrap().result.unwrap().exit_code,
            0,
            "{}",
            cluster.logs()
        );
    }
}

#[tokio::test]
async fn a_worker_runs_several_actions_at_once() {
    let cluster = Cluster::start().await;
    let rendezvous = tempfile::TempDir::new().unwrap();
    let first = meets(rendezvous.path(), "first", "second");
    let second = meets(rendezvous.path(), "second", "first");

    let (first, second) = tokio::join!(cluster.execute(&first), cluster.execute(&second));

    for response in [first.unwrap(), second.unwrap()] {
        assert_eq!(
            response.result.unwrap().exit_code,
            0,
            "each action must see the other running: they did not overlap\n{}",
            cluster.logs()
        );
    }
}

#[tokio::test]
async fn a_write_in_one_request_is_answered_before_the_client_half_closes() {
    let cluster = Cluster::start().await;
    let data = b"one chunk".to_vec();

    assert_eq!(
        cluster.write_in_one_request(&data, 0).await.unwrap(),
        data.len() as i64
    );
    assert_eq!(cluster.read(&digest_of(&data)).await, data);
}

#[tokio::test]
async fn a_write_offset_must_be_what_query_write_status_reports() {
    let cluster = Cluster::start().await;
    let data = b"resumable".to_vec();

    let error = cluster
        .write_in_one_request(&data[4..], 4)
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::InvalidArgument, "{error}");

    cluster.write_in_one_request(&data, 0).await.unwrap();
    let digest = digest_of(&data);
    let resume_after_completion = WriteRequest {
        resource_name: format!("uploads/e2e/blobs/{}/{}", digest.hash, digest.size_bytes),
        write_offset: digest.size_bytes,
        finish_write: true,
        data: Vec::new(),
    };
    let committed =
        ByteStreamClient::with_interceptor(cluster.channel.clone(), cluster.auth.clone())
            .write(futures::stream::iter([resume_after_completion]))
            .await
            .unwrap()
            .into_inner()
            .committed_size;
    assert_eq!(committed, digest.size_bytes);
}

#[tokio::test]
async fn an_action_whose_client_reconnects_is_cached_and_waitable() {
    let cluster = Cluster::start().await;
    let action = TestAction::bash("sleep 1; echo hi > out.txt", &["out.txt"]);

    let mut execute = cluster.submit(&action).await.unwrap();
    let name = execute.message().await.unwrap().unwrap().name;
    drop(execute);

    let response = cluster.wait(name).await.unwrap();
    let result = response.result.unwrap();
    assert_eq!(result.exit_code, 0, "{}", cluster.logs());
    assert_eq!(result.output_files.len(), 1, "{}", cluster.logs());
    let cached = cluster.cached_result(&action).await.unwrap_or_else(|e| {
        panic!(
            "the result of a reconnected client is not cached: {e}\n{}",
            cluster.logs()
        )
    });
    assert_eq!(cached.output_files, result.output_files);
}

#[tokio::test]
async fn a_stdout_above_the_inline_threshold_comes_back_by_digest() {
    const SIZE: usize = 3 * 1024 * 1024;
    let cluster = Cluster::start().await;
    let action = TestAction::bash(&format!("head -c {SIZE} /dev/zero | tr '\\0' x"), &[]);

    let result = cluster.execute(&action).await.unwrap().result.unwrap();

    assert_eq!(result.exit_code, 0, "{}", cluster.logs());
    assert!(
        result.stdout_raw.is_empty(),
        "{} bytes inline",
        result.stdout_raw.len()
    );
    let digest = result
        .stdout_digest
        .clone()
        .expect("stdout stored in the CAS");
    assert_eq!(digest.size_bytes, SIZE as i64);
    assert_eq!(cluster.read(&digest).await, vec![b'x'; SIZE]);
}
