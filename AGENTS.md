# FerrisRBE - AI Coding Agent Guide

> This file contains essential information for AI coding agents working on the FerrisRBE project.
> Read this file before making any changes to the codebase.

## Project Overview

**FerrisRBE** is a high-performance Remote Build Execution (RBE) server for Bazel, implemented in Rust. It implements the Remote Execution v2.4 API (REAPI) providing caching and remote execution capabilities for Bazel builds.

### Key Features
- Full Remote Execution - Dispatches actions to workers in Kubernetes
- Remote Caching - Action cache and CAS (Content Addressable Storage)
- Bidirectional Streaming - Persistent gRPC connection between server and workers
- Auto-scaling - Workers scale automatically (5-100 replicas)
- Version Detection - Adapts responses based on Bazel version
- State Machine - Strict state machine for execution lifecycle
- Multi-level Scheduling - Priority queues for different action types

## Technology Stack

- **Language**: Rust 1.84.0
- **Build System**: Bazel 8.3.0 (with bzlmod)
- **Rust Rules**: rules_rust 0.57.1 with crate_universe
- **Container Rules**: rules_oci 2.2.3
- **gRPC Framework**: Tonic 0.12 (with TLS support)
- **Protobuf**: prost 0.13
- **Async Runtime**: Tokio 1.43 (full features)
- **Concurrency**: DashMap (64 shards), parking_lot, crossbeam
- **Serialization**: serde, prost-types
- **Observability**: tracing, tracing-subscriber
- **Deployment**: OCI containers, Kubernetes

## Project Structure

```
ferrisrbe/
├── MODULE.bazel            # Bazel module definition (bzlmod)
├── .bazelversion           # Bazel version (8.3.0)
├── .bazelrc                # Bazel build configuration
├── Cargo.toml              # Rust configuration - defines rbe-server and rbe-worker binaries
├── Cargo.lock              # Dependency lock file
├── build.rs                # Build script for protobuf code generation
├── Dockerfile              # Server container image (multi-stage build) - legacy
├── Dockerfile.worker       # Worker container image - legacy
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
│   ├── main.rs             # Server binary entry point
│   ├── bin/worker.rs       # Worker binary entry point
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
│   │   ├── engine.rs       # ExecutionEngine (dispatcher, result processor)
│   │   ├── scheduler.rs    # MultiLevelScheduler (Fast/Medium/Slow queues)
│   │   ├── state_machine.rs # ExecutionStateMachine with strict transitions
│   │   ├── results.rs      # ResultsStore for completed executions
│   │   └── output_handler.rs # OutputHandler for large stdout/stderr
│   ├── cas/                # Content Addressable Storage backends
│   │   ├── mod.rs          # CasBackend trait and SharedCasBackend type
│   │   ├── error.rs        # CasError for CAS operations
│   │   └── backends/       # Storage backend implementations
│   │       └── disk.rs     # DiskBackend - filesystem-based storage
│   ├── worker/             # Worker management
│   │   ├── k8s.rs          # WorkerRegistry for Kubernetes workers
│   │   ├── pool.rs         # Worker pool management
│   │   └── multiplex.rs    # Multiplexing for worker connections
│   ├── cache/              # Cache implementations
│   │   └── action_cache.rs # L1ActionCache using DashMap (64 shards)
│   ├── version/            # Bazel version detection
│   │   ├── mod.rs          # VersionManager
│   │   ├── detector.rs     # CompositeDetector for version detection
│   │   ├── registry.rs     # VersionRegistry with handlers
│   │   ├── traits.rs       # BazelVersionHandler trait
│   │   └── handlers/       # Version-specific handlers (v7, v8, v9)
│   └── types.rs            # Core types: DigestInfo, RbeError, etc.
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
├── scripts/                # Utility scripts
│   └── run-local.sh        # Run server locally with cargo
│
├── examples/               # Example Bazel projects for testing
│   ├── bazel-7.4/
│   ├── bazel-8.x/
│   └── bazel-9.x/
```

## Build Commands

### Bazel Build (Recomendado)

Este proyecto ahora se compila con **Bazel 8.3.0** (definido en `.bazelversion`).

#### Primera configuración
```bash
# Instalar Bazelisk (si no lo tienes)
curl -Lo bazelisk https://github.com/bazelbuild/bazelisk/releases/latest/download/bazelisk-linux-amd64
chmod +x bazelisk && sudo mv bazelisk /usr/local/bin/bazel

# Sincronizar dependencias de Rust (primera vez o cuando cambie Cargo.toml)
CARGO_BAZEL_REPIN=1 bazel sync --only=crates
```

#### Compilación básica
```bash
# Compilar todo
bazel build //...

# Compilar solo el servidor
bazel build //:server
# o
bazel build //:rbe-server

# Compilar solo el worker
bazel build //:worker
# o
bazel build //:rbe-worker

# Compilación de release (optimizada)
bazel build --config=release //...

# Compilación rápida para desarrollo
bazel build --config=dev //...
```

#### Construir imágenes de contenedor
```bash
# Construir imágenes
bazel build //oci:server_image //oci:worker_image

# Cargar directamente en Docker (recomendado - usa oci_load)
bazel run //oci:server_load
bazel run //oci:worker_load

# O cargar ambas a la vez
bazel run //oci:load_all

# Alternativa: construir tarball y cargar manualmente
bazel build //oci:server_tarball //oci:worker_tarball
docker load -i bazel-bin/oci/server_tarball/tarball.tar
docker load -i bazel-bin/oci/worker_tarball/tarball.tar
```

#### Ejecutar tests
```bash
# Todos los tests
bazel test //...

# Con output detallado
bazel test --test_output=all //...
```

### Cargo Build (Legacy)

Todavía puedes usar Cargo para desarrollo rápido:

#### Development Build
```bash
cargo build
```

#### Release Build (optimized)
```bash
cargo build --release
```

#### Build Specific Binary
```bash
# Server binary
cargo build --release --bin rbe-server

# Worker binary
cargo build --release --bin rbe-worker
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
# Full deploy to cluster
./k8s/deploy.sh

# Verify pods
kubectl get pods -n rbe

# Port-forward for local access
./k8s/port-forward.sh
```

### Running Tests

```bash
# Run all tests
cargo test

# Run specific test
cargo test test_state_machine_full_flow

# Run with output
cargo test -- --nocapture
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
| DiskBackend | `src/cas/backends/disk.rs` | Filesystem-based CAS implementation |
| OutputHandler | `src/execution/output_handler.rs` | Large output streaming to CAS (AUDIT-1.3) |
| VersionManager | `src/version/mod.rs` | Bazel version detection and handling |

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
         │   DiskBackend         │
         │   (/data/cas)         │
         │                       │
         │  aa/bb/aabbccdd...    │
         └───────────────────────┘
```

**Storage Layout:** Blobs are stored in a content-addressed filesystem structure:
- Path: `<CAS_STORAGE_PATH>/aa/bb/<hash_remainder>`
- Two-level prefix prevents too many files in a single directory
- Atomic writes using temp files + rename

**Streaming Architecture:** For large blob support without OOM:
- ByteStream uploads use bounded channels (4 chunks buffer) for backpressure
- Data flows directly from gRPC stream to disk without accumulating in RAM
- Supports blobs of any size (GBs) with constant memory usage

**Output Streaming (AUDIT-1.3):** For large stdout/stderr from action execution:
- `OutputHandler` processes stdout/stderr before sending to Bazel
- Threshold: 1MB (outputs < 1MB are sent inline, ≥ 1MB stored in CAS)
- Prevents gRPC "message too large" errors and OOM crashes
- ExecutionResponse contains either inline data or CAS digest

**Future Backends:** The trait-based design allows implementing:
- `S3Backend` - for cloud deployments
- `RedisBackend` - for distributed caching
- `TieredBackend` - hot data in memory, cold data on disk

## Environment Variables

### Server Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `RBE_PORT` | `9092` | Server gRPC port |
| `RBE_BIND_ADDRESS` | `0.0.0.0` | Bind address |
| `RUST_LOG` | `info` | Log level (trace/debug/info/warn/error) |
| `CAS_STORAGE_PATH` | `/data/cas` | Local filesystem path for CAS storage |
| `CAS_ENDPOINT` | `bazel-remote:9094` | CAS (bazel-remote) endpoint |
| `REDIS_ENDPOINT` | `redis:6379` | Redis endpoint |

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

### Server Connection Configuration (12-Factor)

Server-side HTTP/2 and gRPC parameters are configurable via environment variables.

| Variable | Default | Description |
|----------|---------|-------------|
| `RBE_TCP_KEEPALIVE_SECS` | `60` | TCP keepalive probe interval |
| `RBE_HTTP2_KEEPALIVE_INTERVAL_SECS` | `60` | HTTP/2 PING frame interval |
| `RBE_HTTP2_KEEPALIVE_TIMEOUT_SECS` | `30` | HTTP/2 PING ACK timeout |
| `RBE_REQUEST_TIMEOUT_SECS` | `600` | Request timeout (0 = disabled) |
| `RBE_HTTP2_ADAPTIVE_WINDOW` | `true` | Enable HTTP/2 adaptive flow control |

**Important:** `RBE_HTTP2_KEEPALIVE_INTERVAL_SECS` on the server should be equal to or greater than worker's `RBE_KEEPALIVE_INTERVAL_SECS` to prevent premature connection drops.

### Worker Connection Configuration (12-Factor)

All connection parameters are configurable via environment variables following 12-Factor App principles. This allows infrastructure operators to tune timeouts without code changes.

| Variable | Default | Description |
|----------|---------|-------------|
| `RBE_KEEPALIVE_INTERVAL_SECS` | `20` | HTTP/2 keepalive ping interval |
| `RBE_MIN_KEEPALIVE_SECS` | `10` | Minimum adaptive keepalive interval |
| `RBE_MAX_KEEPALIVE_SECS` | `60` | Maximum adaptive keepalive interval |
| `RBE_KEEPALIVE_TIMEOUT_SECS` | `15` | HTTP/2 keepalive response timeout |
| `RBE_TCP_KEEPALIVE_SECS` | `30` | TCP keepalive probe interval (should match server) |
| `RBE_CONNECTION_TIMEOUT_SECS` | `30` | TCP connection timeout |
| `RBE_MAX_RECONNECT_ATTEMPTS` | `10` | Max reconnection attempts before shutdown |
| `RBE_RECONNECT_BASE_DELAY_MS` | `100` | Base delay for exponential backoff |
| `RBE_RECONNECT_MAX_DELAY_MS` | `30000` | Max delay for exponential backoff |
| `RBE_RECONNECT_JITTER_FACTOR` | `0.25` | Jitter factor (0.0-1.0) |
| `RBE_HEALTH_CHECK_INTERVAL_SECS` | `5` | Internal health check interval |
| `RBE_HEALTH_CHECK_TIMEOUT_SECS` | `3` | Health check timeout |
| `RBE_EXECUTION_HANDOFF_TIMEOUT_SECS` | `60` | Execution handoff timeout during reconnect |
| `RBE_ADAPTIVE_ADJUSTMENT_THRESHOLD` | `3` | Threshold for adaptive keepalive adjustment |
| `RBE_ENABLE_METRICS` | `true` | Enable connection metrics |

**Note:** The old "Environment Detector" that magically chose configurations based on detecting Docker Desktop, Kubernetes, or Cloud environments has been removed. All configuration is now explicit via environment variables as per RFC_DYNAMIC_CONFIGURATION.md.

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
# Run all tests
cargo test

# Run with verbose output
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

### Docker Build

```bash
# Build server image
docker build -t ferrisrbe/server:latest -f Dockerfile .

# Build worker image
docker build -t ferrisrbe/worker:latest -f Dockerfile.worker .
```

### Kubernetes Resources

The deployment includes:
1. **Namespace**: `rbe`
2. **ConfigMap**: Server configuration
3. **Redis**: Metadata store
4. **bazel-remote**: CAS storage
5. **RBE Server**: gRPC API server (replicas: 1)
6. **RBE Workers**: Auto-scaling worker pool (replicas: 5-100)

### Health Checks

- Server: TCP check on port 9092 (nc -z)
- Workers: Bidirectional gRPC stream health via heartbeats

## Conectividad y Acceso Local

### Opciones de Conectividad

Para desarrollo local con Kubernetes o similares, hay dos opciones para conectar Bazel al cluster:

#### Opción 1: NodePort (RECOMENDADO)

Esta es la opción más estable para conexiones gRPC persistentes.

**Ventajas:**
- No requiere `kubectl port-forward` corriendo
- Conexiones gRPC bidireccionales estables
- No hay timeouts de reconexión

**Puertos expuestos:**
- `localhost:30092` - RBE Server (gRPC)
- `localhost:30094` - Bazel Remote Cache (gRPC)
- `localhost:30080` - Bazel Remote HTTP (opcional)

**Configuración:**
```bash
# Aplicar servicios NodePort (ya incluido en deploy.sh)
./scripts/setup-nodeport.sh apply

# Verificar conectividad
./scripts/setup-nodeport.sh status
```

**Uso en Bazel:**
Los ejemplos en `examples/` ya están configurados para usar NodePort.

```bash
cd examples/bazel-7.4
bazel build //...              # Cache-only (default)
bazel build --config=k8s //...  # Con remote execution
```

#### Opción 2: kubectl port-forward

Alternativa si NodePort no está disponible.

**Desventajas:**
- Requiere mantener procesos corriendo
- Puede tener timeouts con gRPC streaming
- Reconexiones frecuentes

**Configuración:**
```bash
# Terminal 1
kubectl port-forward -n rbe svc/rbe-server 9092:9092

# Terminal 2
kubectl port-forward -n rbe svc/bazel-remote 9094:9094
```

**Uso en Bazel:**
Editar `.bazelrc` y cambiar los puertos de 30092/30094 a 9092/9094.

### Troubleshooting de Conectividad

| Problema | Causa | Solución |
|----------|-------|----------|
| `UNAVAILABLE: io exception` | Puerto no accesible | Verificar NodePort: `nc -zv localhost 30092` |
| `connection refused` | Pods no listos | Esperar a que todos los pods estén Running |
| Workers reconectando | Health check agresivo | Normal, se reconectan automáticamente |
| Timeout en builds | gRPC inestable | Usar NodePort en lugar de port-forward |

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

### Bazel Hangs on "remote"

- Verify workers are registered: `kubectl logs -n rbe -l app=rbe-server | grep "Worker registration"`
- Check worker status: `kubectl get pods -n rbe`
- Verify scheduler has available workers

### Large Output / "Message too large" Errors

The OutputHandler automatically handles large stdout/stderr:
- **Threshold:** 1MB (outputs ≥ 1MB are stored in CAS)
- **Verify handling:** Check server logs for "Output ... is large" or "Stored ... output in CAS"
- **CAS storage:** Large outputs are stored in `/data/cas` with content-addressed paths
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
2. Regenerate code: `cargo build`
3. Update both server (`src/server/worker_service.rs`) and worker (`src/bin/worker.rs`)

## Bazel Configuration

### Estructura del módulo

El proyecto usa **bzlmod** (Bazel Modules) para gestionar dependencias:

| Archivo | Propósito |
|---------|-----------|
| `MODULE.bazel` | Define dependencias del módulo y extensiones |
| `.bazelversion` | Versión exacta de Bazel (8.3.0) |
| `.bazelrc` | Configuraciones de build, flags por defecto |
| `BUILD.bazel` | Targets de Rust (binarios y librerías) |
| `oci/BUILD.bazel` | Definiciones de imágenes de contenedor |

### Dependencias principales (MODULE.bazel)

```starlark
# Reglas de Rust
bazel_dep(name = "rules_rust", version = "0.57.1")

# Protobuf y gRPC
bazel_dep(name = "rules_proto", version = "7.1.0")
bazel_dep(name = "protobuf", version = "29.3")

# Imágenes de contenedor
bazel_dep(name = "rules_oci", version = "2.2.3")
bazel_dep(name = "aspect_bazel_lib", version = "2.14.0")
```

### Gestión de dependencias Rust

Usamos `crate_universe` con el modo `from_cargo`:

```starlark
crate = use_extension("@rules_rust//crate_universe:extensions.bzl", "crate")

crate.from_cargo(
    name = "crates",
    cargo_lockfile = "//:Cargo.lock",
    manifests = ["//:Cargo.toml"],
)

use_repo(crate, "crates")
```

Esto mantiene **Cargo.toml como fuente única de verdad** para las dependencias Rust.

### Comandos útiles de Bazel

```bash
# Ver dependencias del módulo
bazel mod deps

# Ver graph de dependencias
bazel mod graph

# Sincronizar crates (cuando cambia Cargo.toml/Cargo.lock)
CARGO_BAZEL_REPIN=1 bazel sync --only=crates

# Ver todos los targets
bazel query //...

# Ver dependencias de un target
bazel query 'deps(//:rbe-server)'

# Build con profiling
bazel build --profile=/tmp/profile.gz //...
bazel analyze-profile /tmp/profile.gz

# Clean
bazel clean
bazel clean --expunge  # Limpieza completa
```

### Configuraciones disponibles (.bazelrc)

| Configuración | Descripción |
|---------------|-------------|
| `--config=release` | Build optimizado para producción |
| `--config=dev` | Build rápido para desarrollo |
| `--config=debug` | Build con símbolos de debugging |
| `--config=ci` | Configuración para CI/CD |
| `--config=arm64` | Cross-compilación para ARM64 |

## Referencias

## Recent Architecture Changes

### AUDIT-1.1: Unified CAS Backend (Completed)

**Problem:** `CasService` and `ByteStreamService` had separate storage implementations, causing inconsistency.

**Solution:** 
- Created `CasBackend` trait in `src/cas/mod.rs`
- Implemented `DiskBackend` for filesystem storage
- Both services now share `Arc<dyn CasBackend>`

### AUDIT-1.3: Output Streaming to CAS (Completed)

**Problem:** Large stdout/stderr (>4MB) caused gRPC "message too large" errors and OOM crashes.

**Solution:**
- Created `OutputHandler` in `src/execution/output_handler.rs`
- Threshold: 1MB (outputs ≥ 1MB stored in CAS, digest returned)
- ExecutionResponse contains either inline data or CAS digest
- Prevents gRPC message size exceeded errors

### AUDIT-2.3: ByteStream Streaming (Completed)

**Problem:** ByteStream uploads accumulated entire blob in memory before writing to disk.

**Solution:**
- Bounded channel (4 chunks) for backpressure
- Streaming write from gRPC to disk without RAM accumulation
- Constant memory usage (~256KB) regardless of blob size

## References

- [Bazel Documentation](https://bazel.build/docs)
- [rules_rust Documentation](https://bazelbuild.github.io/rules_rust/)
- [rules_oci Documentation](https://github.com/bazel-contrib/rules_oci)
- [Bazel Remote Execution API](https://github.com/bazelbuild/remote-apis)
- [REAPI v2.4 Specification](https://github.com/bazelbuild/remote-apis/blob/main/build/bazel/remote/execution/v2/remote_execution.proto)
- [Tonic gRPC Framework](https://github.com/hyperium/tonic)
- [Tokio Async Runtime](https://tokio.rs/)
