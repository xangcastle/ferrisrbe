# Project Structure

```
ferrisrbe/
├── Cargo.toml              # Rust workspace configuration
├── Cargo.lock              # Dependency lock file
├── build.rs                # Build script (protobuf generation)
├── Dockerfile              # Server container image (legacy)
├── Dockerfile.worker       # Worker container image (legacy)
│
├── oci/                    # OCI container definitions (rules_oci)
│   └── BUILD.bazel         # Multi-platform image definitions
│
├── proto/                  # Protocol Buffers definitions
│   ├── build/bazel/remote/execution/v2/
│   │   └── remote_execution.proto    # REAPI v2.4
│   ├── google/bytestream/
│   │   └── bytestream.proto          # ByteStream API
│   ├── google/longrunning/
│   │   └── operations.proto          # Long-running operations
│   ├── google/rpc/
│   │   ├── status.proto              # RPC status types
│   │   └── error_details.proto       # Error details
│   └── worker.proto                  # WorkerService API
│
├── src/
│   ├── main.rs             # Server binary entry point
│   ├── bin/
│   │   └── worker.rs       # Worker binary with resilient connection
│   ├── server/             # gRPC service implementations
│   │   ├── mod.rs          # RbeServer struct and configuration
│   │   ├── execution_service.rs    # REAPI Execution service
│   │   ├── worker_service.rs       # Bidirectional worker communication
│   │   ├── cas_service.rs          # ContentAddressableStorage service
│   │   ├── action_cache_service.rs # ActionCache service
│   │   ├── byte_stream.rs          # ByteStream service
│   │   ├── capabilities_service.rs # Capabilities service
│   │   └── middleware.rs           # gRPC middleware
│   ├── execution/          # Execution engine
│   │   ├── engine.rs       # ExecutionEngine (dispatcher, results)
│   │   ├── scheduler.rs    # MultiLevelScheduler (priority queues)
│   │   ├── state_machine.rs # ExecutionStateMachine with transitions
│   │   ├── results.rs      # ResultsStore for completed executions
│   │   └── output_handler.rs # Large stdout/stderr handling
│   ├── worker/             # Worker management
│   │   ├── k8s.rs          # WorkerRegistry for Kubernetes
│   │   ├── pool.rs         # Worker pool management
│   │   ├── multiplex.rs    # RequestActor pattern
│   │   └── output_uploader.rs # Output directory upload
│   ├── cas/                # Content Addressable Storage
│   │   ├── mod.rs          # CasBackend trait
│   │   ├── error.rs        # CasError definitions
│   │   └── backends/
│   │       ├── disk.rs     # DiskBackend for local storage
│   │       └── grpc.rs     # GrpcBackend for remote CAS
│   ├── cache/              # Cache implementations
│   │   └── action_cache.rs # L1ActionCache using DashMap
│   ├── version/            # Bazel version detection
│   │   ├── mod.rs          # VersionManager
│   │   ├── detector.rs     # CompositeDetector
│   │   ├── registry.rs     # VersionRegistry
│   │   ├── traits.rs       # BazelVersionHandler trait
│   │   └── handlers/       # Version-specific handlers
│   │       ├── v7.rs
│   │       ├── v8.rs
│   │       └── v9.rs
│   └── types.rs            # Core types: DigestInfo, RbeError
│
├── k8s/                    # Kubernetes manifests
│   ├── namespace.yaml
│   ├── configmap.yaml      # Environment configuration
│   ├── server-deployment.yaml
│   ├── worker-deployment.yaml
│   ├── bazel-remote.yaml   # CAS storage (bazel-remote)
│   ├── redis.yaml          # Metadata store
│   ├── deploy.sh           # Deployment script
│   └── port-forward.sh     # Local port forwarding
│
├── charts/                 # Helm charts
│   └── ferrisrbe/
│       ├── Chart.yaml
│       ├── values.yaml
│       ├── templates/
│       └── README.md
│
├── scripts/                # Utility scripts
│   └── run-local.sh        # Run server locally with cargo
│
└── examples/               # Example Bazel projects for testing
    ├── bazel-7.4/
    ├── bazel-8.x/
    ├── bazel-9.x/
    └── enterprise/         # Stress test suite

```

## Key Directories

### `src/server/`
Contains gRPC service implementations following REAPI v2.4 specification.

### `src/execution/`
The execution engine with state machine, scheduler, and results processing.

### `src/worker/`
Worker management including Kubernetes registry and output handling.

### `src/cas/`
Content Addressable Storage backends with unified trait interface.

### `k8s/`
Production-ready Kubernetes manifests for deployment.

### `charts/`
Helm chart for easy installation and configuration.

## Build System

The project uses **Bazel** for building, with `rules_rust` for Rust compilation and `rules_oci` for container images.

```bash
# Build server and worker
bazel build //:rbe-server //:rbe-worker

# Build container images
bazel run //oci:server_load //oci:worker_load

# Run tests
bazel test //...
```
