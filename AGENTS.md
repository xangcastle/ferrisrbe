# FerrisRBE - AI Coding Agent Guide

> This file contains essential information for AI coding agents working on the FerrisRBE project.
> Read this file before making any changes to the codebase.

## Project Overview

**FerrisRBE** is a high-performance Remote Build Execution (RBE) server for Bazel, implemented in Rust. It implements the Remote Execution v2.4 API (REAPI) providing caching and remote execution capabilities for Bazel builds.

### Key Features
- **Full Remote Execution** - Dispatches actions to workers in Kubernetes
- **Remote Caching** - Action cache and CAS (Content Addressable Storage)
- **Bidirectional Streaming** - Persistent gRPC connection between server and workers
- **Auto-scaling** - Workers scale automatically (configurable replicas)
- **Version Detection** - Adapts responses based on Bazel version
- **State Machine** - Strict state machine for execution lifecycle
- **Multi-level Scheduling** - Priority queues for different action types
- **Resilient Connection** - Adaptive keepalive and automatic reconnection for workers

## Technology Stack

- **Language**: Rust 1.85.0
- **Build System**: Bazel 8.3.0 (with bzlmod)
- **Rust Rules**: rules_rust 0.59.1 with crate_universe
- **Container Rules**: rules_oci 2.2.3
- **gRPC Framework**: Tonic 0.12 (with TLS support)
- **Protobuf**: prost 0.13
- **Async Runtime**: Tokio 1.43 (full features)
- **Concurrency**: DashMap (64 shards), parking_lot, crossbeam
- **Serialization**: serde, prost-types
- **Observability**: tracing, tracing-subscriber
- **Deployment**: OCI containers, Kubernetes, Helm charts

## Project Structure

```
ferrisrbe/
├── MODULE.bazel            # Bazel module definition (bzlmod)
├── .bazelversion           # Bazel version (8.3.0)
├── .bazelrc                # Bazel build configuration
├── Cargo.toml              # Rust configuration - defines rbe-server and rbe-worker binaries
├── Cargo.lock              # Dependency lock file
├── build.rs                # Build script for protobuf code generation
│
├── oci/                    # OCI container image definitions (rules_oci)
│   └── BUILD.bazel         # Image definitions for server and worker
│
├── proto/                  # Protocol Buffers definitions
│   ├── BUILD.bazel         # Bazel exports for proto files
│   ├── worker.proto        # Custom WorkerService API (bidirectional streaming)
│   ├── build/bazel/remote/execution/v2/remote_execution.proto  # REAPI v2.4
│   ├── google/bytestream/bytestream.proto                      # ByteStream API
│   └── google/...          # Standard Google protos (longrunning, rpc, etc.)
│
├── src/
│   ├── lib.rs              # Library entry point
│   ├── main.rs             # Server binary entry point
│   ├── bin/worker.rs       # Worker binary entry point
│   ├── bin/resilient_connection/  # Worker connection management modules
│   │   ├── mod.rs          # Module exports
│   │   ├── connection_manager.rs  # Connection lifecycle management
│   │   ├── connection_state.rs    # Connection state machine
│   │   ├── adaptive_keepalive.rs  # Adaptive keepalive tuning
│   │   ├── health_checker.rs      # Health checking
│   │   ├── metrics.rs             # Connection metrics
│   │   └── reconnection.rs        # Reconnection strategies
│   ├── server/             # gRPC service implementations
│   │   ├── mod.rs          # RbeServer struct and configuration
│   │   ├── execution_service.rs    # REAPI Execution service
│   │   ├── worker_service.rs       # Bidirectional worker communication
│   │   ├── cas_service.rs          # ContentAddressableStorage service
│   │   ├── action_cache_service.rs # ActionCache service
│   │   ├── byte_stream.rs          # ByteStream service (upload/download)
│   │   ├── capabilities_service.rs # Capabilities service (REAPI discovery)
│   │   └── middleware.rs           # gRPC middleware
│   ├── execution/          # Execution engine
│   │   ├── mod.rs          # Module exports
│   │   ├── engine.rs       # ExecutionEngine (dispatcher, result processor)
│   │   ├── scheduler.rs    # MultiLevelScheduler (Fast/Medium/Slow queues)
│   │   ├── state_machine.rs # ExecutionStateMachine with strict transitions
│   │   ├── results.rs      # ResultsStore for completed executions
│   │   └── output_handler.rs # OutputHandler for large stdout/stderr
│   ├── cas/                # Content Addressable Storage backends
│   │   ├── mod.rs          # CasBackend trait and SharedCasBackend type
│   │   ├── error.rs        # CasError for CAS operations
│   │   └── backends/       # Storage backend implementations
│   │       ├── disk.rs     # DiskBackend - filesystem-based storage
│   │       ├── grpc.rs     # GrpcCasBackend - gRPC-based CAS proxy
│   │       ├── http_proxy.rs # HTTP proxy backend
│   │       └── mod.rs      # Backend exports
│   ├── worker/             # Worker management
│   │   ├── mod.rs          # Module exports, ActionResult, WorkerId
│   │   ├── k8s.rs          # WorkerRegistry for Kubernetes workers
│   │   ├── pool.rs         # Worker pool management
│   │   ├── multiplex.rs    # Multiplexing for worker connections
│   │   ├── materializer.rs # Merkle tree materialization for execroot
│   │   └── output_uploader.rs # Output uploading to CAS
│   ├── cache/              # Cache implementations
│   │   ├── mod.rs          # Module exports
│   │   └── action_cache.rs # L1ActionCache using DashMap (64 shards)
│   ├── version/            # Bazel version detection
│   │   ├── mod.rs          # VersionManager
│   │   ├── detector.rs     # CompositeDetector for version detection
│   │   ├── registry.rs     # VersionRegistry with handlers
│   │   ├── traits.rs       # BazelVersionHandler trait
│   │   └── handlers/       # Version-specific handlers (v7, v8, v9)
│   └── types.rs            # Core types: DigestInfo, RbeError, AtomicInstant
│
├── k8s/                    # Kubernetes manifests
│   ├── deploy.sh           # Deployment script
│   ├── port-forward.sh     # Local port forwarding
│   ├── namespace.yaml
│   ├── configmap.yaml
│   ├── server-deployment.yaml
│   ├── worker-deployment.yaml
│   ├── bazel-remote.yaml   # CAS storage (bazel-remote)
│   └── redis.yaml          # Metadata store
│
├── charts/                 # Helm charts
│   └── ferrisrbe/
│       ├── Chart.yaml
│       ├── values.yaml     # Configuration values
│       └── templates/      # Kubernetes templates
│
├── scripts/                # Utility scripts
│   └── run-local.sh        # Run server locally with cargo
│
├── examples/               # Example Bazel projects for testing
│   ├── bazel-7.4/
│   ├── bazel-8.x/
│   └── bazel-9.x/
│
└── docs/                   # Documentation
    ├── architecture.md     # System architecture
    ├── configuration.md    # Environment variables
    ├── deployment.md       # Deployment guide
    ├── bazel-integration.md # Bazel configuration
    └── troubleshooting.md  # Common issues
```

## Build Commands

### Bazel Build (Recommended)

This project builds with **Bazel 8.3.0** (defined in `.bazelversion`).

#### First Time Setup
```bash
# Install Bazelisk (if you don't have it)
curl -Lo bazelisk https://github.com/bazelbuild/bazelisk/releases/latest/download/bazelisk-linux-amd64
chmod +x bazelisk && sudo mv bazelisk /usr/local/bin/bazel

# Sync Rust dependencies (first time or when Cargo.toml changes)
CARGO_BAZEL_REPIN=1 bazel sync --only=crates
```

#### Basic Build Commands
```bash
# Build everything
bazel build //...

# Build server only
bazel build //:server
# or
bazel build //:rbe-server

# Build worker only
bazel build //:worker
# or
bazel build //:rbe-worker

# Release build (optimized)
bazel build --config=release //...

# Development build (fast)
bazel build --config=dev //...

# Debug build
bazel build --config=debug //...
```

#### Build Container Images
```bash
# Build images
bazel build //oci:server_image //oci:worker_image

# Load directly into Docker (uses oci_load)
bazel run //oci:server_load
bazel run //oci:worker_load

# Load both at once
bazel run //oci:load_all

# Push images (requires Docker credentials)
bazel run //oci:server_push
bazel run //oci:worker_push
```

#### Run Tests
```bash
# All tests
bazel test //...

# With verbose output
bazel test --test_output=all //...

# Specific test
bazel test //:rbe_lib_test
```

### Cargo Build (Development)

For rapid development, you can still use Cargo:

```bash
# Development build
cargo build

# Release build (optimized)
cargo build --release

# Build specific binary
cargo build --release --bin rbe-server
cargo build --release --bin rbe-worker

# Run tests
cargo test

# Run with output
cargo test -- --nocapture
```

#### Protobuf Code Generation
The protobuf code is automatically generated during build via `build.rs`:
- Worker proto generates both client and server code
- REAPI protos generate server-only code

To force regeneration, touch the source files:
```bash
touch src/main.rs && touch src/bin/worker.rs
```

## Running and Testing

### Local Development

Use the provided script:
```bash
./scripts/run-local.sh
```

Or manually:
```bash
export RBE_PORT=9092
export RBE_BIND_ADDRESS=127.0.0.1
export RUST_LOG=info
cargo run --release
```

### Kubernetes Deployment

```bash
# Full deploy using Helm
helm install ferrisrbe ./charts/ferrisrbe --namespace rbe --create-namespace

# Or using raw manifests
./k8s/deploy.sh

# Verify pods
kubectl get pods -n rbe

# Port-forward for local access
./k8s/port-forward.sh
```

### Docker Compose (Local Testing)

```bash
# Start all services
docker-compose up -d

# View logs
docker-compose logs -f

# Stop
docker-compose down
```

## Architecture Overview

### Execution Flow

1. **Bazel** sends an action to `ExecutionService` via gRPC (REAPI v2.4)
2. **ExecutionService** creates a state machine via `StateMachineManager`
3. **L1ActionCache** is checked for existing results (cache hit short-circuits to response)
4. **MultiLevelScheduler** enqueues the action:
   - Fast queue: actions < 1MB input
   - Medium queue: actions 1MB-100MB input
   - Slow queue: actions > 100MB input
5. **ExecutionEngine** dispatcher dequeues the action and transitions state to `Assigned`
6. **WorkerRegistry** selects an available idle worker
7. **WorkerService** sends `WorkAssignment` to the worker via bidirectional gRPC stream
8. **Worker** executes the action and returns `ExecutionResult`
9. **ExecutionEngine** result processor stores the result in `ResultsStore`
10. State machine transitions to `Completed` or `Failed`, result is returned to Bazel

### State Machine

Execution stages (strict transitions enforced):
```
CacheCheck → Queued → Assigned → Downloading → Executing → Uploading → Completed
     ↓            ↓          ↓            ↓           ↓           ↓
  Failed       Failed     Failed       Failed      Failed      Failed
```

### Core Components

| Component | File | Responsibility |
|-----------|------|----------------|
| RbeServer | `src/server/mod.rs` | Main server, configures and starts all gRPC services |
| ExecutionEngine | `src/execution/engine.rs` | Dispatcher loop, result processor, cleanup |
| MultiLevelScheduler | `src/execution/scheduler.rs` | Priority queuing with action merging |
| ExecutionStateMachine | `src/execution/state_machine.rs` | State tracking and transitions |
| WorkerRegistry | `src/worker/k8s.rs` | Worker registration and selection |
| L1ActionCache | `src/cache/action_cache.rs` | In-memory action result cache (DashMap) |
| CasBackend | `src/cas/mod.rs` | Unified CAS storage trait for all services |
| GrpcCasBackend | `src/cas/backends/grpc.rs` | gRPC-based CAS proxy to bazel-remote |
| OutputHandler | `src/execution/output_handler.rs` | Large output streaming to CAS |
| VersionManager | `src/version/mod.rs` | Bazel version detection and handling |
| ConnectionManager | `src/bin/resilient_connection/connection_manager.rs` | Worker connection lifecycle |
| Materializer | `src/worker/materializer.rs` | Execroot materialization with Merkle trees |

### CAS (Content Addressable Storage) Architecture

FerrisRBE uses a **unified CAS backend** that is shared between `CasService` and `ByteStreamService`. This ensures consistency: a blob uploaded via `BatchUpdateBlobs` is immediately available via `ByteStream.Read`, and vice versa.

```
┌─────────────────┐     ┌─────────────────┐
│  CasService     │     │ ByteStream      │
│  (REAPI gRPC)   │     │ Service         │
└────────┬────────┘     └────────┬────────┘
         │                       │
         └───────────┬───────────┘
                     │
         ┌───────────▼───────────┐
│   CasBackend Trait    │
│   (Arc<dyn Trait>)    │
└───────────┬───────────┘
                     │
         ┌───────────▼───────────┐
│   GrpcCasBackend      │
│   (bazel-remote)      │
└───────────────────────┘
```

**Storage Backend Options:**
- **DiskBackend** - Filesystem-based storage (local CAS)
- **GrpcCasBackend** - gRPC proxy to external CAS (bazel-remote)
- **HttpProxyBackend** - HTTP proxy backend

### Resilient Worker Connection

The worker implements enterprise-grade connection management:

- **Adaptive Keepalive** - Adjusts intervals based on network conditions
- **Exponential Backoff** - Reconnection with jitter and configurable delays
- **Execution Handoff** - Preserves executions during reconnection
- **Health Checking** - Bidirectional health checks with timeouts
- **Connection State Machine** - Tracks states: Disconnected → Connecting → Connected → Ready

## Environment Variables

### Server Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `RBE_PORT` | `9092` | Server gRPC port |
| `RBE_BIND_ADDRESS` | `0.0.0.0` | Bind address |
| `RUST_LOG` | `info` | Log level (trace/debug/info/warn/error) |
| `CAS_ENDPOINT` | `bazel-remote:9094` | CAS (bazel-remote) endpoint |
| `REDIS_ENDPOINT` | `redis:6379` | Redis endpoint |
| `RBE_L1_CACHE_CAPACITY` | `100000` | L1 cache capacity |
| `RBE_L1_CACHE_TTL_SECS` | `3600` | L1 cache TTL |
| `RBE_INLINE_OUTPUT_THRESHOLD` | `1048576` | Inline output threshold (1MB) |

### Server HTTP/2 Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `RBE_TCP_KEEPALIVE_SECS` | `30` | TCP keepalive probe interval |
| `RBE_HTTP2_KEEPALIVE_INTERVAL_SECS` | `20` | HTTP/2 PING frame interval |
| `RBE_HTTP2_KEEPALIVE_TIMEOUT_SECS` | `15` | HTTP/2 PING ACK timeout |
| `RBE_REQUEST_TIMEOUT_SECS` | `600` | Request timeout (0 = disabled) |
| `RBE_HTTP2_ADAPTIVE_WINDOW` | `true` | Enable HTTP/2 adaptive flow control |

### Worker Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `WORKER_ID` | (generated) | Unique worker ID (falls back to HOSTNAME) |
| `SERVER_ENDPOINT` | `http://rbe-server:9092` | RBE server endpoint |
| `CAS_ENDPOINT` | `http://bazel-remote:9094` | CAS endpoint |
| `WORKER_TYPE` | `default` | Worker type (default, highcpu, gpu, etc.) |
| `WORKER_LABELS` | `os=linux,arch=amd64` | Comma-separated labels |
| `MAX_CONCURRENT` | `4` | Maximum concurrent executions |
| `WORKDIR` | `/workspace` | Working directory for executions |

### Worker Connection Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `RBE_KEEPALIVE_INTERVAL_SECS` | `20` | HTTP/2 keepalive ping interval |
| `RBE_MIN_KEEPALIVE_SECS` | `10` | Minimum adaptive keepalive interval |
| `RBE_MAX_KEEPALIVE_SECS` | `60` | Maximum adaptive keepalive interval |
| `RBE_KEEPALIVE_TIMEOUT_SECS` | `15` | HTTP/2 keepalive response timeout |
| `RBE_TCP_KEEPALIVE_SECS` | `30` | TCP keepalive probe interval |
| `RBE_CONNECTION_TIMEOUT_SECS` | `30` | TCP connection timeout |
| `RBE_MAX_RECONNECT_ATTEMPTS` | `10` | Max reconnection attempts |
| `RBE_RECONNECT_BASE_DELAY_MS` | `100` | Base delay for exponential backoff |
| `RBE_RECONNECT_MAX_DELAY_MS` | `30000` | Max delay for exponential backoff |
| `RBE_RECONNECT_JITTER_FACTOR` | `0.25` | Jitter factor (0.0-1.0) |
| `RBE_HEALTH_CHECK_INTERVAL_SECS` | `5` | Health check frequency |
| `RBE_HEALTH_CHECK_TIMEOUT_SECS` | `3` | Health check timeout |
| `RBE_EXECUTION_HANDOFF_TIMEOUT_SECS` | `60` | Execution handoff timeout |
| `RBE_ADAPTIVE_ADJUSTMENT_THRESHOLD` | `3` | Adaptive adjustment threshold |
| `RBE_ENABLE_METRICS` | `true` | Enable connection metrics |
| `RBE_PRINT_CONFIG_OPTIONS` | (unset) | Print all config options on startup |

### Materializer Settings

| Variable | Default | Description |
|----------|---------|-------------|
| `RBE_DOWNLOAD_TIMEOUT_SECS` | `300` | Per-file download timeout |
| `RBE_MAX_CONCURRENT_DOWNLOADS` | `10` | Max concurrent downloads |
| `RBE_DOWNLOAD_CHUNK_SIZE` | `65536` | Download chunk size (64KB) |
| `RBE_STREAMING_THRESHOLD` | `4194304` | Streaming threshold (4MB) |
| `RBE_USE_HARDLINKS` | `true` | Use hardlinks when possible |
| `RBE_VALIDATE_DIGESTS` | `true` | Validate digests after download |

**Important:** Server HTTP/2 interval/timeout must be >= worker values to prevent connection drops.

## Code Style Guidelines

### Rust Conventions

- Use `snake_case` for functions and variables
- Use `PascalCase` for types, traits, and structs
- Use `SCREAMING_SNAKE_CASE` for constants
- Max line length: 100 characters (enforced by rustfmt)

### Error Handling

- Use `thiserror` for defining error types (see `RbeError` in `types.rs`)
- Use `anyhow` for application-level error handling
- Prefer `Result<T, RbeError>` for library code

### Async/Await

- Use `tokio` for async runtime
- Use `Arc` for shared state across tasks
- Use `RwLock` for read-heavy shared data
- Use `Mutex` (from `parking_lot`) for short critical sections

### Comments

- Code comments in English
- Some protocol buffer comments in Spanish (legacy)
- Doc comments (`///`) for public APIs

### Generated Code

Generated protobuf code in `OUT_DIR` should have clippy suppressions:
```rust
#[allow(clippy::doc_lazy_continuation)]
pub mod proto { ... }
```

## Testing Strategy

### Unit Tests

Tests are located inline in source files within `#[cfg(test)]` modules:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_feature() {
        // Test implementation
    }
}
```

### Integration Tests

Integration tests are in `src/main.rs` under `mod integration_tests`:
- `test_full_stack_compilation` - Verifies all modules compile
- `test_state_machine_full_flow` - Tests state machine transitions
- `test_dashmap_sharding` - Verifies cache sharding

### Test Commands

```bash
# Run all Bazel tests
bazel test //...

# Run all Cargo tests
cargo test

# Run with verbose output
bazel test --test_output=all //...
cargo test -- --nocapture

# Run specific test
cargo test test_state_machine_full_flow
```

## Security Considerations

### gRPC Security

- Server supports TLS (configured via Tonic)
- Current deployment uses plaintext for internal cluster communication
- For production, enable TLS with proper certificates

### Worker Isolation

- Workers run in separate Kubernetes pods
- Each worker has a dedicated workspace directory
- Resource limits should be configured in Kubernetes manifests

### CAS Security

- bazel-remote (CAS) should run within the cluster
- No external exposure of CAS endpoint
- Authentication can be added at the gRPC middleware layer

## Deployment

### Docker Compose

```bash
# Start all services (uses pre-built images from Docker Hub)
docker-compose up -d

# Or build locally with Bazel first, then run
bazel run //oci:load_all
docker-compose up -d
```

**Note:** docker-compose.yml currently uses image tag `0.1.0-test-amd64`. Update to `latest` after creating a release tag.

### Railway Deployment (Remote Cache Only)

Deploy the FerrisRBE server to Railway for **Remote Cache** functionality:

```bash
# Deploy cache-only server
railway service create ferrisrbe-cache --source .
```

**⚠️ Limitation:** Railway deployment only provides Remote Cache (Action Cache + CAS). 
**Remote Execution requires workers** which must be deployed separately using Docker Compose or Kubernetes.

**Environment Variables:**

| Variable | Default | Description |
|----------|---------|-------------|
| `RBE_PORT` | `${PORT}` | Server port (auto-assigned by Railway) |
| `RUST_LOG` | `info` | Log level |

**Current Image:** `0.1.0-test-amd64` (update to `latest` after release)

**For Full Remote Execution**, use Docker Compose or Helm locally:
```bash
# Local full RBE with workers
docker-compose up -d
```

### Helm Deployment (Recommended)

```bash
# Install with Helm
helm install ferrisrbe ./charts/ferrisrbe \
  --namespace rbe \
  --create-namespace

# With NodePort for local testing
helm install ferrisrbe ./charts/ferrisrbe \
  --namespace rbe \
  --create-namespace \
  --set server.service.type=NodePort \
  --set server.service.nodePort=30092

# Upgrade
helm upgrade ferrisrbe ./charts/ferrisrbe --namespace rbe

# Uninstall
helm uninstall ferrisrbe --namespace rbe
```

### Health Checks

- Server: TCP check on port 9092
- Workers: Bidirectional gRPC stream health via heartbeats
- Liveness/Readiness probes configured in Helm charts

## Connectivity and Local Access

### NodePort (Recommended for Local Development)

Puertos expuestos:
- `localhost:30092` - RBE Server (gRPC)
- `localhost:30094` - Bazel Remote Cache (gRPC)
- `localhost:30080` - Bazel Remote HTTP (optional)

```bash
# Check connectivity
nc -zv localhost 30092
nc -zv localhost 30094
```

### Bazel Configuration

Add to your `.bazelrc`:

```bash
# Remote Cache
build:remote-cache --remote_cache=grpc://localhost:30092
build:remote-cache --remote_upload_local_results=true

# Remote Execution
build:remote-exec --config=remote-cache
build:remote-exec --remote_executor=grpc://localhost:30092
build:remote-exec --remote_default_exec_properties=OSFamily=linux
```

## Troubleshooting

### Workers Not Connecting

```bash
kubectl logs -n rbe -l app=rbe-worker --tail=50
kubectl get svc -n rbe
```

### Bazel Cannot Find Server

```bash
# Verify connectivity
grpcurl -plaintext localhost:9092 build.bazel.remote.execution.v2.Capabilities/GetCapabilities

# Check server logs
kubectl logs -n rbe -l app=rbe-server --tail=50
```

### HTTP/2 Connection Drops

If you see "error reading a body from connection":
1. Ensure server keepalive interval >= worker keepalive interval
2. Check that `RBE_HTTP2_KEEPALIVE_INTERVAL_SECS` on server >= `RBE_KEEPALIVE_INTERVAL_SECS` on worker
3. Verify TCP keepalive settings match on both sides

### Bazel Hangs on "remote"

- Verify workers are registered: `kubectl logs -n rbe -l app=rbe-server | grep "Worker registration"`
- Check worker status: `kubectl get pods -n rbe`
- Verify scheduler has available workers

### Large Output / "Message too large" Errors

The OutputHandler automatically handles large stdout/stderr:
- **Threshold:** 1MB (outputs >= 1MB are stored in CAS)
- **Verify handling:** Check server logs for "Output ... is large" or "Stored ... output in CAS"
- **CAS storage:** Large outputs are stored in bazel-remote
- **gRPC limit:** Tonic default is 4MB; OutputHandler prevents exceeding this

## Protocol Buffer APIs

### REAPI Services (v2.4)

- **Execution** - Execute actions remotely (`Execute`, `WaitExecution`)
- **ContentAddressableStorage** - Blob storage by digest (`FindMissingBlobs`, `BatchReadBlobs`, `BatchUpdateBlobs`)
- **ActionCache** - Action results cache (`GetActionResult`, `UpdateActionResult`)
- **ByteStream** - Large blob upload/download (`Read`, `Write`)
- **Capabilities** - Server capability discovery (`GetCapabilities`)

### Worker Service (Custom)

Bidirectional streaming for worker management:
- **StreamWork** - Persistent gRPC stream
  - Worker → Server: `WorkerRegistration`, `WorkerHeartbeat`, `ExecutionUpdate`, `ExecutionResult`
  - Server → Worker: `RegistrationAck`, `WorkAssignment`, `CancelExecution`

## Common Tasks for AI Agents

### Adding a New gRPC Service

1. Define the service in a `.proto` file under `proto/`
2. Update `build.rs` if needed
3. Implement the service in `src/server/<service_name>.rs`
4. Register the service in `src/server/mod.rs` in `RbeServer::run()`

### Adding a New Execution Stage

1. Add the stage to `ExecutionStage` enum in `src/execution/state_machine.rs`
2. Update `as_str()` method
3. Update `can_transition_to()` to define valid transitions
4. Add tests for the new transitions

### Adding Version-Specific Behavior

1. Create a new handler in `src/version/handlers/`
2. Implement `BazelVersionHandler` trait
3. Register the handler in `src/version/registry.rs`

### Modifying Worker Protocol

1. Update `proto/worker.proto`
2. Regenerate code: `cargo build` or `bazel build //...`
3. Update both server (`src/server/worker_service.rs`) and worker (`src/bin/worker.rs`)

### Adding a New CAS Backend

1. Implement `CasBackend` trait in `src/cas/backends/<name>.rs`
2. Add to `src/cas/backends/mod.rs`
3. Update `RbeServer::new()` in `src/server/mod.rs` to use the new backend if needed

## Bazel Configuration Reference

### Module Structure

The project uses **bzlmod** (Bazel Modules) for dependency management:

| File | Purpose |
|------|---------|
| `MODULE.bazel` | Define module dependencies and extensions |
| `.bazelversion` | Exact Bazel version (8.3.0) |
| `.bazelrc` | Build configurations, default flags |
| `BUILD.bazel` | Rust targets (binaries and libraries) |
| `oci/BUILD.bazel` | Container image definitions |

### Useful Bazel Commands

```bash
# View module dependencies
bazel mod deps

# View dependency graph
bazel mod graph

# Sync crates (when Cargo.toml/Cargo.lock changes)
CARGO_BAZEL_REPIN=1 bazel sync --only=crates

# View all targets
bazel query //...

# View dependencies of a target
bazel query 'deps(//:rbe-server)'

# Build with profiling
bazel build --profile=/tmp/profile.gz //...
bazel analyze-profile /tmp/profile.gz

# Clean
bazel clean
bazel clean --expunge  # Full cleanup
```

### Available Configurations (.bazelrc)

| Configuration | Description |
|---------------|-------------|
| `--config=release` | Optimized production build |
| `--config=dev` | Fast development build |
| `--config=debug` | Debug build with symbols |
| `--config=ci` | CI/CD configuration |
| `--config=linux_amd64` | Cross-compile for AMD64 |
| `--config=arm64` | Cross-compile for ARM64 |

## Documentation

Full documentation is available in the `docs/` directory:

- `docs/architecture.md` - System design and components
- `docs/deployment.md` - Kubernetes, Helm, and Docker deployment
- `docs/configuration.md` - Environment variables and tuning
- `docs/bazel-integration.md` - `.bazelrc` configuration
- `docs/api.md` - REAPI v2.4 endpoints
- `docs/monitoring.md` - Metrics and logging
- `docs/troubleshooting.md` - Common issues and solutions
- `docs/project-structure.md` - Codebase orientation

## References

- [Bazel Documentation](https://bazel.build/docs)
- [rules_rust Documentation](https://bazelbuild.github.io/rules_rust/)
- [rules_oci Documentation](https://github.com/bazel-contrib/rules_oci)
- [Bazel Remote Execution API](https://github.com/bazelbuild/remote-apis)
- [REAPI v2.4 Specification](https://github.com/bazelbuild/remote-apis/blob/main/build/bazel/remote/execution/v2/remote_execution.proto)
- [Tonic gRPC Framework](https://github.com/hyperium/tonic)
- [Tokio Async Runtime](https://tokio.rs/)
