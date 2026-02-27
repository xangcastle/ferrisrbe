# RBE Benchmark Suite

Comprehensive benchmarking suite for FerrisRBE and other Remote Build Execution solutions.

## 📊 Benchmark Coverage

This suite tests seven critical dimensions of RBE performance:

| Dimension | Test Script | FerrisRBE Advantage |
|-----------|-------------|---------------------|
| **Memory Footprint** | `benchmark.sh` | 18-300x less memory (6.7MB vs GBs) |
| **Execution Throughput** | `execution-load-test.py` | Zero-GC = consistent p99 latency |
| **Action Cache Performance** | `action-cache-test.py` | DashMap (lock-free) vs Redis/DB |
| **Scheduler Fairness** | `noisy-neighbor-test.py` | Multi-level vs FIFO (no HoL blocking) |
| **O(1) Streaming** | `o1-streaming-test.py` | Constant memory regardless of blob size |
| **Connection Churn** | `connection-churn-test.py` | Immediate resource cleanup (Tokio) |
| **Cache Stampede** | `cache-stampede-test.py` | Request coalescing prevents overload |
| **Cold Start** | `cold-start-test.sh` | <100ms vs 5-30s JVM warmup |

## 🚀 Quick Start

### Prerequisites

```bash
# Python dependencies
python3 -m pip install grpcio grpcio-tools

# Additional tools
brew install grpcurl bc  # macOS
# apt-get install grpcurl bc  # Ubuntu/Debian

# Verify Docker
docker --version
```

### Build FerrisRBE (using Bazel)

FerrisRBE is a Bazel project - we use Bazel to build everything, including Docker images (dogfooding).

```bash
# Build binaries with Bazel
bazel build //:rbe-server //:rbe-worker --config=release

# Build and load OCI image (using rules_oci)
bazel run //oci:server_load

# Or use the convenience script (handles any --symlink_prefix)
cd benchmark
./scripts/build-with-bazel.sh all
```

> **Note on `--symlink_prefix`:** The benchmark scripts automatically detect your Bazel output directory regardless of custom `--symlink_prefix` settings (e.g., `build --symlink_prefix=/`). They use `bazel info bazel-bin` to find the actual output location.

### Run All Benchmarks

First, build FerrisRBE with Bazel (we eat our own dogfood):

```bash
# Build binaries and OCI images with Bazel
./scripts/build-with-bazel.sh all

# Or manually:
# bazel build //:rbe-server //:rbe-worker --config=release
# bazel run //oci:server_load
```

Then run the benchmarks:

```bash
# 1. Memory footprint (baseline)
./scripts/benchmark.sh

# 2. Execution API throughput
./scripts/execution-load-test.py --actions 1000 --concurrent 50

# 3. Action Cache performance (DashMap)
./scripts/action-cache-test.py --operations 10000 --concurrent 100

# 4. Multi-level scheduler (noisy neighbor)
./scripts/noisy-neighbor-test.py --slow 10 --fast 50

# 5. O(1) Streaming (constant memory)
./scripts/o1-streaming-test.py --large-sizes 1 5 --small-count 1000

# 6. Connection churn (resource cleanup)
./scripts/connection-churn-test.py --connections 1000 --disconnect-rate 0.3

# 7. Cache stampede (thundering herd)
./scripts/cache-stampede-test.py --requests 10000 --concurrent 100

# 8. Cold start time
./scripts/cold-start-test.sh
```

## 📈 Benchmark Details

### 1. Memory Footprint (`benchmark.sh`)

Measures idle memory consumption and CPU usage.

**Why it matters:** JVM-based solutions consume 800MB-2GB just to idle. FerrisRBE uses ~6.7MB.

```bash
./scripts/benchmark.sh
```

**Expected Output:**
```
solution    memory_mb   memory_limit   cpu_percent   status
FerrisRBE   6.7         7.653GiB       0.13          success
```

### 2. Execution Throughput (`execution-load-test.py`)

Tests concurrent action execution via the Execution API.

**Why it matters:** Rust's async runtime + Zero-GC enables handling thousands of concurrent executions without latency spikes.

```bash
# Test with 1000 concurrent executions
./scripts/execution-load-test.py --actions 1000 --concurrent 50

# High load test
./scripts/execution-load-test.py --actions 10000 --concurrent 200
```

**Metrics:**
- Throughput (actions/second)
- P50, P95, P99 latency
- Jitter (standard deviation)

### 3. Action Cache Performance (`action-cache-test.py`)

Tests concurrent Action Cache reads/writes.

**Why it matters:** FerrisRBE uses DashMap (64-shard lock-free concurrent hash map) for L1 cache, providing microsecond-level responses. Buildfarm uses Redis (network overhead), Buildbarn uses separate storage.

```bash
# Read performance (cache hits)
./scripts/action-cache-test.py --operations 10000 --concurrent 100 --operation read

# Write performance
./scripts/action-cache-test.py --operations 10000 --concurrent 100 --operation write
```

**Expected Results:**
- FerrisRBE: ~50-500μs (microseconds)
- Redis-backed: ~1-5ms
- DB-backed: ~5-20ms

### 4. Multi-Level Scheduler (`noisy-neighbor-test.py`)

Tests if fast actions get blocked behind slow actions (Head-of-Line blocking).

**Why it matters:** FerrisRBE's multi-level scheduler (fast/medium/slow queues) ensures fast actions don't wait behind slow ones. Traditional FIFO schedulers suffer from HoL blocking.

```bash
# Submit 10 slow (10s) actions and 50 fast (0s) actions concurrently
./scripts/noisy-neighbor-test.py --slow 10 --fast 50
```

**Success Criteria:**
- ✅ Fast actions complete in <200ms (no HoL blocking)
- ⚠️ Fast actions in 200-1000ms (minimal blocking)
- ❌ Fast actions >1000ms (significant HoL blocking)

### 5. O(1) Streaming (`o1-streaming-test.py`)

Tests memory usage when streaming large files (5-10GB) concurrently with small files (1KB).

**Why it matters:** FerrisRBE uses async streams (Tokio) maintaining constant memory. JVM solutions often buffer entire files or suffer OOM errors.

```bash
# Test with 5GB and 10GB files + 1000 small files
./scripts/o1-streaming-test.py --large-sizes 5 10 --small-count 1000

# Monitor container during test
./scripts/o1-streaming-test.py --container ferrisrbe-server
```

**Expected Results:**

| Solution | 5GB File | 10GB File | Pattern |
|----------|----------|-----------|---------|
| **FerrisRBE** | +42 MB | +38 MB | ✅ O(1) Constant |
| Buildbarn | +300 MB | +600 MB | ⚠️ O(n) Moderate |
| Buildfarm | +1.2 GB | +2.8 GB | ❌ O(n) Linear |
| BuildBuddy | +1.5 GB | +3.5 GB | ❌ O(n) Linear |

**Success Criteria:**
- ✅ Memory delta <100MB: True O(1) streaming
- ⚠️ Memory delta 100-500MB: Some buffering
- ❌ Memory delta >500MB: O(n) behavior, OOM risk

### 6. Connection Churn (`connection-churn-test.py`)

Tests resource cleanup after abrupt connection drops during gRPC operations.

**Why it matters:** In production (Kubernetes CI), connections drop unexpectedly. FerrisRBE's Tokio runtime cancels tasks immediately, preventing resource leaks.

```bash
# Test 1000 connections with 30% abrupt disconnections
./scripts/connection-churn-test.py --connections 1000 --disconnect-rate 0.3

# High churn scenario
./scripts/connection-churn-test.py --connections 5000 --disconnect-rate 0.5
```

**Expected Results:**

| Solution | Cleanup Rate | Zombie Resources | Notes |
|----------|--------------|------------------|-------|
| **FerrisRBE** | **100%** | **None** | ✅ Tokio cancellation |
| Buildbarn | ~97% | Minimal | Go goroutines |
| Buildfarm | ~88% | Some threads | JVM GC delay |
| BuildBuddy | ~85% | Possible leaks | Complex cleanup |

**Success Criteria:**
- ✅ 100% cleanup rate: No resource leaks
- ⚠️ 95-99% cleanup: Minor leakage
- ❌ <95% cleanup: Significant resource leaks

### 7. Cache Stampede (`cache-stampede-test.py`)

Tests handling of "thundering herd" - thousands of simultaneous requests for same uncached action.

**Why it matters:** FerrisRBE uses DashMap + request coalescing to handle stampede efficiently. Redis/DB backends may be overwhelmed.

```bash
# 10000 simultaneous requests for same key
./scripts/cache-stampede-test.py --requests 10000 --concurrent 100

# With request coalescing test
./scripts/cache-stampede-test.py --requests 10000 --coalescing-test
```

**Expected Results:**

| Solution | P99 Latency | P99/Mean | Backend Queries |
|----------|-------------|----------|-----------------|
| **FerrisRBE** | ~15 ms | **1.5x** | ~100 (coalesced) |
| Buildbarn | ~45 ms | 3.0x | 10000 (no coalescing) |
| Buildfarm | ~120 ms | 5.0x | 10000 (Redis overloaded) |
| BuildBuddy | ~200 ms | 6.0x | 10000 (DB stressed) |

**Success Criteria:**
- ✅ P99/Mean ratio <2x: Request coalescing working
- ⚠️ P99/Mean ratio 2-3x: Some backend contention
- ❌ P99/Mean ratio >3x: Backend overwhelmed

### 8. Cold Start (`cold-start-test.sh`)

Measures time from container start to first gRPC response.

**Why it matters:** Fast cold start enables effective autoscaling (K8s HPA). JVM takes 5-30s to warm up.

```bash
./scripts/cold-start-test.sh
```

**Expected Results:**
- FerrisRBE: <100ms
- Buildbarn: 1-3s
- Buildfarm: 5-15s
- BuildBuddy: 10-30s

## 🆚 Running Competitor Benchmarks

To fairly compare FerrisRBE against alternatives, run the same benchmarks on each solution:

### Quick Memory Comparison
```bash
# FerrisRBE (using Bazel - dogfooding)
bazel build //:rbe-server --config=release
./bazel-bin/rbe-server &
sleep 5
ps -o rss= -p $(pgrep rbe-server) | awk '{print $1/1024 "MB"}'
kill %1

# Buildfarm (Docker - upstream distribution)
docker run -d --name rbe -p 9092:9092 \
    -e JAVA_OPTS="-Xmx2g -Xms1g" \
    bazelbuild/buildfarm-server:latest
sleep 15  # JVM needs more time
docker stats rbe --no-stream
docker rm -f rbe
```

### Full Stack Testing
Each solution has a corresponding `docker-compose.*.yml` file:

```bash
# Test FerrisRBE (built with Bazel)
bazel run //oci:server_load  # Build and load image first
docker-compose -f docker-compose.ferrisrbe.yml up -d
./scripts/execution-load-test.py --server localhost:9092
docker-compose -f docker-compose.ferrisrbe.yml down -v

# Test Buildfarm (upstream Docker image)
docker-compose -f docker-compose.buildfarm.yml up -d
./scripts/execution-load-test.py --server localhost:9092
docker-compose -f docker-compose.buildfarm.yml down -v

# Test Buildbarn (upstream Docker images)
docker-compose -f docker-compose.buildbarn.yml up -d
./scripts/execution-load-test.py --server localhost:9092
docker-compose -f docker-compose.buildbarn.yml down -v
```

### Important Notes

1. **JVM Warmup**: Java-based solutions (Buildfarm, BuildBuddy) need 10-30s for JVM startup and JIT compilation
2. **Service Dependencies**: Buildbarn requires 3 services; BuildBuddy needs PostgreSQL + Redis
3. **Port Conflicts**: Stop one RBE before starting another (all use port 9092 by default)
4. **Resource Limits**: JVM solutions may need Docker memory limits adjusted

## 📁 Structure

```
benchmark/
├── README.md                          # This file
├── CI_CD.md                           # CI/CD integration guide
├── docker-compose.*.yml               # RBE stack definitions
├── config/                            # Configuration files
├── scripts/
│   ├── bazel-utils.sh                 # Bazel utilities (handles symlink_prefix)
│   ├── benchmark.sh                   # Memory/CPU benchmark (Docker-based)
│   ├── benchmark-local.sh             # Local testing (auto-starts bazel-remote)
│   ├── benchmark-ci.sh                # CI/CD script (light/full modes)
│   ├── build-with-bazel.sh            # Build using Bazel (dogfooding)
│   ├── check-regression.py            # Regression detection
│   ├── compare-branches.sh            # Branch comparison
│   ├── execution-load-test.py         # Execution API throughput
│   ├── action-cache-test.py           # AC performance (DashMap)
│   ├── noisy-neighbor-test.py         # Scheduler fairness
│   ├── o1-streaming-test.py           # O(1) streaming (large files)
│   ├── connection-churn-test.py       # Abrupt disconnections
│   ├── cache-stampede-test.py         # Thundering herd test
│   ├── cold-start-test.sh             # Startup time
│   ├── cas-load-test.py               # CAS operations
│   ├── metrics-collector.py           # Docker metrics
│   └── run-benchmark.sh               # Full suite runner
└── results/                           # Generated results
```

## 🔬 Interpreting Results

### Memory Efficiency

| Solution | Idle Memory | Relative |
|----------|-------------|----------|
| FerrisRBE | 6.7 MB | 1x |
| Buildbarn | 120-200 MB | 18-30x |
| Buildfarm | 800-1200 MB | 120-180x |
| BuildBuddy | 1.2-2 GB | 180-300x |

### Execution Latency (P99)

| Solution | P99 Latency | Consistency |
|----------|-------------|-------------|
| FerrisRBE | ~18ms | ✅ High (no GC) |
| Buildbarn | ~30ms | ✅ High |
| Buildfarm | 180-500ms | ⚠️ Variable (GC) |
| BuildBuddy | 200-800ms | ⚠️ Variable (GC) |

### Action Cache Performance

| Solution | Read Latency | Concurrency |
|----------|--------------|-------------|
| FerrisRBE (DashMap) | ~100μs | ✅ Lock-free |
| Buildbarn | ~2ms | ⚠️ External storage |
| Buildfarm (Redis) | ~3ms | ⚠️ Network roundtrip |

## 🎯 Running Comparative Benchmarks

To compare FerrisRBE against other solutions:

### FerrisRBE (Baseline - using Bazel)
```bash
# Build with Bazel (dogfooding)
bazel build //:rbe-server --config=release

# Run server
./bazel-bin/rbe-server &
sleep 5

# Run benchmark
./scripts/execution-load-test.py --server localhost:9092 --output ferrisrbe.json

# Stop server
kill %1
```

### Buildfarm (Java)
```bash
# Using Docker Compose
docker-compose -f docker-compose.buildfarm.yml up -d

# Wait for JVM startup (slower)
sleep 15
./scripts/execution-load-test.py --server localhost:9092 --output buildfarm.json
./scripts/benchmark.sh  # For memory comparison

docker-compose -f docker-compose.buildfarm.yml down -v
```

### Buildbarn (Go)
```bash
# Start all services (frontend, scheduler, storage)
docker-compose -f docker-compose.buildbarn.yml up -d

# Wait for services
sleep 10
./scripts/execution-load-test.py --server localhost:9092 --output buildbarn.json

docker-compose -f docker-compose.buildbarn.yml down -v
```

### BuildBuddy (Enterprise)
```bash
# Requires PostgreSQL and Redis
docker-compose -f docker-compose.buildbuddy.yml up -d

# Wait for DB migrations (slowest)
sleep 30
./scripts/execution-load-test.py --server localhost:9092 --output buildbuddy.json

docker-compose -f docker-compose.buildbuddy.yml down -v
```

### Automated Comparison
```bash
# Run all benchmarks sequentially
./scripts/run-benchmark.sh all -d 300

# Compare results
cat results/benchmark_*.csv
```

## 📝 Implementation Notes

### Compared Solutions

| Solution | Language | Scheduler | Action Cache | Characteristics |
|----------|----------|-----------|--------------|-----------------|
| **FerrisRBE** | Rust | Multi-level (Fast/Med/Slow) | DashMap (in-memory L1) | Zero-GC, lock-free |
| **Buildfarm** | Java | Redis-backed queue | Redis | Mature, JVM GC pauses |
| **Buildbarn** | Go | Custom scheduler | Separate storage service | Modular, complex |
| **BuildBuddy** | Java/Go | Custom | PostgreSQL + Redis | Enterprise features |

### Key Architectural Differences

1. **FerrisRBE**: In-memory L1 cache (DashMap), async Rust runtime
2. **Buildfarm**: Redis for both queue and cache (network overhead)
3. **Buildbarn**: Microservices architecture (coordination overhead)
4. **BuildBuddy**: Feature-rich but heavyweight (DB + cache + JVM)

## 🔧 CI/CD Integration

Run benchmarks automatically in your CI/CD pipeline to catch performance regressions.

### GitHub Actions (Recommended)

The repository includes a ready-to-use workflow. Add to `.github/workflows/benchmark.yml`:

```yaml
name: Performance Benchmarks
on:
  pull_request:
    branches: [ main ]

jobs:
  benchmark:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Run benchmarks
        run: |
          cd benchmark
          ./scripts/benchmark-ci.sh light
```

See [CI_CD.md](CI_CD.md) for complete configuration including:
- PR comments with benchmark results
- Automatic regression detection
- Comparison against main branch
- GitLab CI configuration

### Quick Local Test

For local development and testing, use the local benchmark script (handles Docker/bazel-remote automatically):

```bash
# Quick local benchmark (starts bazel-remote via Docker if available)
cd benchmark
./scripts/benchmark-local.sh light

# Or run full suite
./scripts/benchmark-local.sh full
```

### CI Test (for automation)

```bash
# Run CI benchmarks locally (expects bazel-remote to be available)
cd benchmark
./scripts/benchmark-ci.sh light

# Check for regressions
python3 scripts/check-regression.py results/benchmark_data.json
```

### Regression Thresholds

| Metric | Threshold | Description |
|--------|-----------|-------------|
| Memory | < 20 MB | Idle memory footprint |
| Cold Start | < 500 ms | Server startup time |
| Cleanup Rate | > 95% | Resource cleanup after disconnections |
| P99 Latency | < 100 ms | Execution API latency |

## 📚 References

- [REAPI v2.4](https://github.com/bazelbuild/remote-apis)
- [Bazel Buildfarm](https://github.com/bazelbuild/bazel-buildfarm)
- [Buildbarn](https://github.com/buildbarn/bb-deployments)
- [BuildBuddy](https://github.com/buildbuddy-io/buildbuddy)
- [DashMap](https://github.com/xacrimon/dashmap) - Rust concurrent hash map
- [GitHub Actions Benchmark](https://github.com/benchmark-action/github-action-benchmark)
