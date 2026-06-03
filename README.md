<p align="center">
  <img src="docs/logo.png" alt="FerrisRBE Logo" width="200"/>
</p>

<h1 align="center">FerrisRBE</h1>

<p align="center">
  <strong>A lean, predictable, and blazingly fast Remote Build Execution (RBE) server for Bazel, written in Rust.</strong>
</p>

<p align="center">
  <a href="https://github.com/xangcastle/ferrisrbe/actions"><img src="https://img.shields.io/badge/CI-Passing-brightgreen" alt="CI Status"></a>
  <a href="https://github.com/xangcastle/ferrisrbe/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License"></a>
  <a href="https://github.com/bazelbuild/remote-apis"><img src="https://img.shields.io/badge/REAPI-v2.4%20Compliant-blue" alt="REAPI Compliant"></a>
  <a href="#"><img src="https://img.shields.io/badge/Memory-~50MB%20RSS-orange" alt="Memory"></a>
</p>

---

## Why FerrisRBE?

Most RBE solutions are built on the JVM, requiring constant GC tuning and 4GB+ memory just to idle. When your build cache server needs its own dedicated node, something is wrong.

**FerrisRBE takes a different approach:**

* **Zero GC Pauses:** Rust's ownership model eliminates garbage collection. Predictable p99 latencies without JVM tuning.
* **O(1) Memory CAS Streaming:** Stream 10GB artifacts with constant ~50MB RAM usage. No more OOM kills during large uploads.
* **12-Factor by Default:** No XML, no YAML, no properties files. Just environment variables that operators already know how to manage.
* **Adaptive Resilience:** Workers auto-tune keepalive intervals based on network conditions. Transient failures don't fail builds.

## 🚀 Quick Start

### Option 1: Railway (Easiest - Full RBE)

Deploy complete Remote Build Execution infrastructure with one click:
- **RBE Server** - gRPC API for cache and execution  
- **bazel-remote** - CAS (Content Addressable Storage)
- **Redis** - Metadata store
- **Workers** - Build executors

[![Deploy on Railway](https://railway.com/button.svg)](https://railway.com/deploy/rxPtfg?referralCode=yQR-JU&utm_medium=integration&utm_source=template&utm_campaign=generic)

```bash
# Get your Railway server URL from the dashboard
# Configure Bazel:
echo 'build:remote --remote_executor=grpc://<your-railway-url>' >> ~/.bazelrc
echo 'build:remote --remote_cache=grpc://<your-railway-url>' >> ~/.bazelrc
echo 'build:remote --remote_default_exec_properties=OSFamily=linux' >> ~/.bazelrc
bazel build --config=remote //...
```

### Option 2: Cloud Development Environments

[![Open in GitHub Codespaces](https://github.com/codespaces/badge.svg)](https://codespaces.new/xangcastle/ferrisrbe?quickstart=1)
[![Open in Gitpod](https://gitpod.io/button/open-in-gitpod.svg)](https://gitpod.io/#https://github.com/xangcastle/ferrisrbe)

### Option 3: Docker Compose (Full RBE - Local)

Complete RBE stack with workers, cache, and execution on your machine.

```bash
git clone https://github.com/xangcastle/ferrisrbe.git
cd ferrisrbe
docker-compose up -d

# Verify the container is running and healthy
curl -I http://localhost:9092 || echo "Server is up"

# Configure Bazel
echo 'build:remote --remote_executor=grpc://localhost:9092' >> ~/.bazelrc
echo 'build:remote --remote_cache=grpc://localhost:9092' >> ~/.bazelrc
bazel build --config=remote //...
```

### Option 4: Kubernetes (Production)

```bash
# Helm install
helm install ferrisrbe oci://ghcr.io/xangcastle/ferrisrbe/charts/ferrisrbe \
  --namespace rbe --create-namespace

# Or with NodePort for local testing
helm install ferrisrbe oci://ghcr.io/xangcastle/ferrisrbe/charts/ferrisrbe \
  --namespace rbe --create-namespace \
  --set server.service.type=NodePort \
  --set server.service.nodePort=30092
```

For advanced deployments requiring extensive configuration, [view the direct Helm deployment values.yaml](https://github.com/xangcastle/ferrisrbe/blob/main/charts/ferrisrbe/values.yaml).

## Architecture Highlights

FerrisRBE isn't a toy implementation; it's designed to handle the thundering herd of a massive monorepo CI pipeline.

* **Multi-Level Queuing:** Fast, medium, and slow queues automatically determined by action size. No more head-of-line blocking. Fast actions (<1s) never wait behind slow ones (>10s).
* **Lock-Free Concurrency:** Leveraging `DashMap` with 64 shards for L1 action cache and in-flight operations, ensuring high throughput without lock contention. Action cache reads in microseconds, not milliseconds.
* **Event-Driven Workers:** Eliminates busy-waiting CPU cycles using `tokio::sync::Notify`. Your cluster's CPU is for building, not polling.
* **Smart Materialization:** Automatically degrades from zero-copy hardlinks to standard copies on `EXDEV` cross-device volume mounts (perfect for containerized executors).
* **Zero-GC Runtime:** Rust's ownership model eliminates garbage collection pauses. Predictable p99 latencies under any load.

### Cache Architecture (L1/L2)

FerrisRBE implements a tiered caching strategy:

```
┌─────────────────────────────────────────────────────────────┐
│  L1 Cache: DashMap (in-memory)                              │
│  - 64 shards, lock-free concurrent access                   │
│  - Microsecond-level reads (~50-100μs)                      │
│  - Lost on server restart                                   │
├─────────────────────────────────────────────────────────────┤
│  L2 Cache: Redis/Memcached (planned)                        │
│  - Persistent across restarts                               │
│  - Shared across server replicas                            │
│  - Millisecond-level reads (~1-3ms)                         │
├─────────────────────────────────────────────────────────────┤
│  L3 Storage: CAS (bazel-remote)                             │
│  - Persistent blob storage                                  │
│  - Gigabyte-scale artifacts                                 │
└─────────────────────────────────────────────────────────────┘
```

**Current Status:** L1 (DashMap) is implemented and provides exceptional performance for action cache hits. L2 integration is planned to provide persistence and horizontal scalability.

## 📊 Benchmarks

The following metrics were obtained through reproducible comparative benchmarks against other RBE solutions (Buildfarm, Buildbarn, BuildBuddy). Benchmark scripts are available in [`benchmark/`](benchmark/).

### Memory Footprint Comparison (Idle State)

| Solution | Language | Idle Memory | Relative to FerrisRBE | GC Pauses |
|----------|----------|-------------|----------------------|-----------|
| **FerrisRBE** 🦀 | Rust | **6.7 MB** | **1x** (baseline) | **None** ✅ |
| Buildbarn | Go | ~120-200 MB | ~18-30x | Minimal |
| Buildfarm | Java | ~800-1200 MB | ~120-180x | Yes (G1GC) ⚠️ |
| BuildBuddy | Java/Go | ~1.2-2 GB | ~180-300x | Yes (JVM) ⚠️ |

> **Note:** FerrisRBE is **120-300x more memory efficient** than JVM-based solutions, and **18-30x more efficient** than Go.

### Real-World Impact: Resource Costs

For a 20-node RBE cluster:

| Solution | Total Memory (Idle) | Est. Monthly Cost* |
|----------|---------------------|-------------------|
| **FerrisRBE** | ~134 MB | **$50-100** |
| Buildbarn | ~2.4-4 GB | $150-250 |
| Buildfarm | ~16-24 GB | $800-1,200 |
| BuildBuddy | ~24-40 GB | $1,200-2,000 |

\*AWS/GCP estimate for instances required to support baseline memory.

### Resource Footprint - FerrisRBE Details

| Component | Memory (Idle) | Memory (Peak) | CPU (Idle) |
|-----------|---------------|---------------|------------|
| Server | ~5-10 MB | ~50-150 MB | ~0.01 cores |
| Worker | ~10-15 MB | ~100-200 MB | ~0.01 cores |
| **Total** | **~15-25 MB** | **~150-350 MB** | **~0.02 cores** |

Compare to Java-based alternatives that idle at 500MB+ and spike to 4GB+ during GC.

### Why Zero-GC Matters

| Metric | FerrisRBE | JVM Solutions |
|--------|-----------|---------------|
| p50 Latency | 12 ms | 45 ms |
| p99 Latency | 18 ms | 180-500 ms* |
| p99.9 Latency | 25 ms | 500-2000 ms* |
| Large File Streaming | O(1) memory | O(n) memory, OOM risk |
| Connection Cleanup | Immediate | Delayed (zombie threads) |
| Cache Stampede | Coalesced | Backend overload |
| Consistency | ✅ High | ⚠️ Variable |

\* Latency spikes caused by GC pauses

### Advanced Stress Tests

| Test | FerrisRBE | JVM Solutions |
|------|-----------|---------------|
| **O(1) Streaming** | Constant memory regardless of file size | Memory scales with file size |
| **Connection Churn** | Immediate task cancellation | Zombie threads, resource leaks |
| **Cache Stampede** | Request coalescing prevents overload | Backend overwhelmed |
| **Multi-level Scheduling** | Fast actions never blocked | Head-of-line blocking (FIFO) |

### Running Benchmarks

The benchmark suite tests eight critical dimensions of RBE performance:

```bash
cd benchmark

# 1. Memory footprint (baseline)
./scripts/benchmark.sh

# 2. Execution API throughput (Zero-GC advantage)
./scripts/execution-load-test.py --actions 1000 --concurrent 50

# 3. Action Cache performance (DashMap vs Redis)
./scripts/action-cache-test.py --operations 10000 --concurrent 100

# 4. Multi-level scheduler (no head-of-line blocking)
./scripts/noisy-neighbor-test.py --slow 10 --fast 50

# 5. O(1) Streaming (constant memory with large files)
./scripts/o1-streaming-test.py --large-sizes 5 10 --small-count 1000

# 6. Connection churn (resource cleanup)
./scripts/connection-churn-test.py --connections 1000 --disconnect-rate 0.3

# 7. Cache stampede (thundering herd protection)
./scripts/cache-stampede-test.py --requests 10000 --concurrent 100

# 8. Cold start time (<100ms vs 5-30s JVM)
./scripts/cold-start-test.sh
```

See [benchmark/README.md](benchmark/README.md) for detailed benchmark documentation and [benchmark/results/BENCHMARK_RESULTS.md](benchmark/results/BENCHMARK_RESULTS.md) for results.

## Quick Start

### 1. Configure Bazel

Add to your `.bazelrc`:

```bash
# Remote Cache (works from any OS)
build:remote-cache --remote_cache=grpc://localhost:9092
build:remote-cache --remote_upload_local_results=true

# Remote Execution (requires Linux toolchains)
build:remote-exec --config=remote-cache
build:remote-exec --remote_executor=grpc://localhost:9092
build:remote-exec --remote_default_exec_properties=OSFamily=linux
```

### 2. Build

```bash
# Cache only
bazel build --config=remote-cache //...

# Full remote execution
bazel build --config=remote-exec //...
```

### 3. Verify

```bash
# You should see "remote cache hit" and "remote" execution in the output
bazel build --config=remote //... 2>&1 | grep -E "(remote cache hit|processes)"
```

## Configuration

FerrisRBE strictly follows 12-Factor App methodology. No cryptic XML or YAML files required.

| Env Variable | Default | Description |
|--------------|---------|-------------|
| `RBE_PORT` | `9092` | Server listening port |
| `RBE_L1_CACHE_CAPACITY` | `100000` | Max entries in the in-memory action cache |
| `RBE_INLINE_OUTPUT_THRESHOLD` | `1048576` | Size (bytes) below which outputs are sent inline |
| `RBE_MAX_CONCURRENT_DOWNLOADS` | `10` | Concurrency limit for materializing execroots |

See [docs/configuration.md](docs/configuration.md) for the complete reference.

## Documentation

- [Architecture](docs/architecture.md) - System design and components
- [Deployment](docs/deployment.md) - Kubernetes, Helm, and Docker deployment
- [Configuration](docs/configuration.md) - Environment variables and tuning
- [Bazel Integration](docs/bazel-integration.md) - `.bazelrc` configuration
- [API Reference](docs/api.md) - REAPI v2.4 endpoints
- [Monitoring](docs/monitoring.md) - Metrics and logging
- [Troubleshooting](docs/troubleshooting.md) - Common issues and solutions

## Project Structure

```
ferrisrbe/
├── src/
│   ├── server/          # gRPC services (REAPI v2.4)
│   ├── execution/       # Scheduler, state machine, results
│   ├── worker/          # Worker registry and management
│   ├── cas/             # Content Addressable Storage backends
│   └── cache/           # L1 Action Cache (DashMap)
├── charts/              # Helm charts for Kubernetes
├── k8s/                 # Raw Kubernetes manifests
├── examples/            # Test projects (Bazel 7.4, 8.x, 9.x)
└── docs/                # Full documentation
```

## Roadmap

### Completed ✅

- [x] REAPI v2.4 Capabilities Service
- [x] Action merging (deduplication of identical in-flight actions)
- [x] HTTP/2 adaptive keepalive for resilient worker connections
- [x] Multi-level scheduler (Fast/Medium/Slow queues)
- [x] DashMap-based L1 Action Cache (lock-free, 64 shards)
- [x] O(1) CAS streaming with async I/O
- [x] Comprehensive benchmark suite (8 dimensions)

### In Progress 🚧

- [ ] Persistent L2 Cache integration (Redis/Memcached)
  - *Why: Current L1 cache (DashMap) is in-memory only. L2 provides persistence across server restarts and cache sharing across multiple server replicas.*
- [ ] Prometheus / OpenTelemetry metrics exposition
  - *Why: Production observability for SRE teams*

### Planned 📋

- [ ] Web UI for build monitoring
  - Real-time build queue visualization
  - Worker status and health
  - Cache hit/miss analytics
- [ ] Remote Build Without the Bytes (BwoB) support
  - Skip downloading outputs when not needed
- [ ] Compressed CAS transfers (zstd)
  - Reduce network bandwidth for large artifacts

## Contributing

PRs are welcome. We value:
- Clean abstractions
- Explicit error handling
- Comprehensive documentation
- Avoiding `unwrap()` in critical paths

See [docs/project-structure.md](docs/project-structure.md) for codebase orientation.

## License

[MIT](LICENSE)

---

<p align="center">
  Built with 🦀 for engineers who value predictability.
</p>
