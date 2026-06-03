# RBE Benchmark Suite - Container-Native

Comprehensive benchmarking suite for FerrisRBE that tests **actual OCI containers** (like production deployment), not just native binaries.

## 🎯 Why Container-Native Benchmarking?

Traditional benchmarks run binaries directly, but FerrisRBE is designed to run in Kubernetes. Container-native benchmarking provides:

| Aspect | Binary Mode | Container Mode (This) |
|--------|-------------|----------------------|
| **Realism** | ❌ Tests binary only | ✅ Tests actual OCI image deployed to K8s |
| **Networking** | ❌ localhost direct | ✅ Docker networking stack |
| **Resources** | ❌ Unlimited host resources | ✅ CPU/memory limits like K8s |
| **Cold Start** | ❌ Process startup only | ✅ Container + process startup |
| **Image Issues** | ❌ Can't detect OCI build bugs | ✅ Validates entire build pipeline |

## 📊 Benchmark Coverage

This suite tests seven critical dimensions of RBE performance:

| Dimension | Test Script | FerrisRBE Advantage |
|-----------|-------------|---------------------|
| **Memory Footprint** | `benchmark-ci.sh` | 18-300x less memory (6.7MB vs GBs) |
| **Execution Throughput** | `execution-load-test.py` | Zero-GC = consistent p99 latency |
| **Action Cache Performance** | `action-cache-test.py` | DashMap (lock-free) vs Redis/DB |
| **Scheduler Fairness** | `noisy-neighbor-test.py` | Multi-level vs FIFO (no HoL blocking) |
| **O(1) Streaming** | `o1-streaming-test.py` | Constant memory regardless of blob size |
| **Connection Churn** | `connection-churn-test.py` | Immediate resource cleanup (Tokio) |
| **Cache Stampede** | `cache-stampede-test.py` | Request coalescing prevents overload |
| **Cold Start** | `benchmark-ci.sh` | <100ms vs 5-30s JVM warmup |

## 🚀 Quick Start (Container Mode)

### Prerequisites

```bash
# Docker (required)
docker --version

# Python dependencies
python3 -m pip install grpcio grpcio-tools

# Bazel (to build images)
bazel --version
```

### Build and Run

```bash
# 1. Build OCI image with Bazel (dogfooding)
bazel run //oci:server_load_amd64

# 2. Run container-native benchmarks
cd benchmark
./scripts/benchmark-ci.sh light

# Or run full suite
./scripts/benchmark-ci.sh full
```

### Using Docker Compose (Alternative)

```bash
# Start services
docker-compose -f docker-compose.benchmark.yml up -d

# Run benchmarks against running container
./scripts/execution-load-test.py --server localhost:9092

# Cleanup
docker-compose -f docker-compose.benchmark.yml down -v
```

### Generate Report from Results

After running benchmarks, generate a comprehensive markdown report:

```bash
# Generate report from all JSON results
./scripts/generate-report.sh

# Or specify custom paths
./scripts/generate-report.sh ../results ../results/my-report.md
```

This creates:
- `results/BENCHMARK_REPORT_<timestamp>.md` - Full report with tables and analysis
- `results/LATEST_REPORT.md` - Symlink to the most recent report

The report includes:
- Executive summary with pass/fail status
- Detailed results for each benchmark
- Comparison with official release (if available)
- Threshold analysis and recommendations

## 🆚 Compare Against Official Release

Instead of compiling `main` branch (slow), compare your PR against the **official Docker Hub image**:

```bash
# Compare PR image against xangcastle/ferris-server:latest
./scripts/compare-branches.sh ferrisrbe/server:latest
```

This is **much faster** (~30s vs ~5-10min) and tests the **actual artifact users run**.

### Example Output

```markdown
## 📊 Performance Comparison: PR vs Official Release

| Metric | Official (latest) | PR | Change | Status |
|--------|-------------------|-----|--------|--------|
| Memory (MB) | 6.5 | 6.7 | ↑ 3% | ✅ |
| Image Size | 45MB | 44MB | ↓ 2% | ✅ |

#### Legend
- ✅ Within 5% - Acceptable
- ⚠️ 5-15% change - Review recommended
- 🚨 >15% regression - Optimization required
```

## 📁 Structure

```
benchmark/
├── README.md                          # This file
├── docker-compose.benchmark.yml       # Optimized benchmark setup
├── docker-compose.ferrisrbe.yml       # Full stack (dev)
├── config/                            # Configuration files
├── scripts/
│   ├── benchmark-ci.sh               # ⭐ Main CI script (container-native)
│   ├── compare-branches.sh           # ⭐ Compare PR vs official release
│   ├── generate-report.sh            # ⭐ Generate markdown report from results
│   ├── check-regression.py           # Regression detection
│   ├── execution-load-test.py        # Execution API throughput
│   ├── action-cache-test.py          # AC performance (DashMap)
│   ├── noisy-neighbor-test.py        # Scheduler fairness
│   ├── o1-streaming-test.py          # O(1) streaming (large files)
│   ├── connection-churn-test.py      # Abrupt disconnections
│   ├── cache-stampede-test.py        # Thundering herd test
│   └── ...
└── results/                           # Generated results
    ├── BENCHMARK_REPORT_*.md         # Auto-generated reports
    └── LATEST_REPORT.md              # Symlink to latest report
```

## 🐕 Dogfooding with Bazel

FerrisRBE uses **rules_oci** to build OCI images, not Dockerfiles:

```bash
# Build AMD64 image (for Linux/CI)
bazel run //oci:server_load_amd64

# Build ARM64 image (for Mac M1/M2)
bazel run //oci:server_load

# The benchmark scripts use this image automatically
./scripts/benchmark-ci.sh light
```

## 🔬 Benchmark Details

### 1. Memory Footprint (Container)

Measures idle memory consumption inside the container with resource limits.

```bash
./scripts/benchmark-ci.sh light  # Includes memory test
```

**Why container mode matters:** Tests actual memory usage with Docker limits, not host OS memory.

### 2. Cold Start (Container)

Measures container startup + server ready time.

```bash
./scripts/benchmark-ci.sh light  # Includes cold start test
```

**Container cold start includes:**
- Docker container creation
- Process startup
- gRPC server ready

### 3. Execution Throughput

Tests concurrent action execution via the Execution API against the container.

```bash
./scripts/execution-load-test.py \
    --server localhost:9092 \
    --actions 1000 \
    --concurrent 50
```

### 4. O(1) Streaming with Container Monitoring

Tests memory usage when streaming large files, monitoring actual container stats:

```bash
./scripts/o1-streaming-test.py \
    --server localhost:9092 \
    --large-sizes 5 10 \
    --small-count 1000 \
    --container ferrisrbe-benchmark-server
```

## 🔄 CI/CD Integration

The repository includes a GitHub Actions workflow (`.github/workflows/benchmark.yml`) that:

1. **Builds** OCI image using Bazel
2. **Runs** container-native benchmarks
3. **Compares** against `xangcastle/ferris-server:latest`
4. **Comments** results on PRs
5. **Fails** on performance regressions

### Key Improvements in Container Mode

| Before (Binary) | After (Container) |
|-----------------|-------------------|
| Compiles `main` branch (~5-10min) | Pulls `latest` image (~30s) |
| Tests binary only | Tests actual OCI image |
| No resource limits | Enforces K8s-like limits |
| Can't detect image bugs | Validates full build pipeline |

## 📈 Interpreting Results

### Memory Efficiency

| Solution | Idle Memory | Container Overhead | Total |
|----------|-------------|-------------------|-------|
| **FerrisRBE** | 6.7 MB | ~2 MB | ~9 MB |
| Buildbarn | 120-200 MB | ~5 MB | ~125-205 MB |
| Buildfarm | 800-1200 MB | ~10 MB | ~810-1210 MB |

### Cold Start Time

| Solution | Container Startup | Total |
|----------|------------------|-------|
| **FerrisRBE** | <100ms | <1s |
| Buildbarn | 1-3s | 2-5s |
| Buildfarm | 5-15s | 10-25s |

## 🛠️ Running Comparative Benchmarks

### FerrisRBE (Local Build)

```bash
# Build with Bazel
bazel run //oci:server_load_amd64

# Run benchmarks
./scripts/benchmark-ci.sh light
```

### Against Official Release

```bash
# Compare local build against Docker Hub
./scripts/compare-branches.sh ferrisrbe/server:latest
```

### Other Solutions

Each solution has a corresponding `docker-compose.*.yml` file:

```bash
# Test Buildfarm (upstream Docker image)
docker-compose -f docker-compose.buildfarm.yml up -d
./scripts/execution-load-test.py --server localhost:9092
docker-compose -f docker-compose.buildfarm.yml down -v

# Test Buildbarn (upstream Docker images)
docker-compose -f docker-compose.buildbarn.yml up -d
./scripts/execution-load-test.py --server localhost:9092
docker-compose -f docker-compose.buildbarn.yml down -v
```

## 📝 Implementation Notes

### Compared Solutions

| Solution | Language | Scheduler | Action Cache | Characteristics |
|----------|----------|-----------|--------------|-----------------|
| **FerrisRBE** | Rust | Multi-level (Fast/Med/Slow) | DashMap (in-memory L1) | Zero-GC, lock-free |
| **Buildfarm** | Java | Redis-backed queue | Redis | Mature, JVM GC pauses |
| **Buildbarn** | Go | Custom scheduler | Separate storage service | Modular, complex |
| **BuildBuddy** | Java/Go | Custom | PostgreSQL + Redis | Enterprise features |

### Why Container-Native is Better for FerrisRBE

1. **Production Parity:** Tests the exact image deployed to K8s
2. **Build Validation:** Catches issues in the OCI build process
3. **Realistic Cold Start:** Includes container runtime overhead
4. **Resource Testing:** Validates behavior under constraints

## 🔧 Regression Thresholds

| Metric | Threshold | Description |
|--------|-----------|-------------|
| Memory | < 20 MB | Idle memory footprint (containerized) |
| Cold Start | < 500 ms | Container startup + server ready |
| Execution P99 | < 100 ms | Execution API latency |
| Cache P99 | < 1000 μs | Cache read P99 |
| Cleanup Rate | > 95% | Resource cleanup after disconnections |

## 📚 References

- [REAPI v2.4](https://github.com/bazelbuild/remote-apis)
- [Bazel Buildfarm](https://github.com/bazelbuild/bazel-buildfarm)
- [Buildbarn](https://github.com/buildbarn/bb-deployments)
- [BuildBuddy](https://github.com/buildbuddy-io/buildbuddy)
- [rules_oci](https://github.com/bazel-contrib/rules_oci) - Bazel OCI rules
- [DashMap](https://github.com/xacrimon/dashmap) - Rust concurrent hash map
- [GitHub Actions Benchmark](https://github.com/benchmark-action/github-action-benchmark)
