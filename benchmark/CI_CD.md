# CI/CD Integration Guide

This guide explains how to integrate the FerrisRBE benchmark suite into your CI/CD pipeline to prevent performance regressions.

## Overview

The benchmark suite provides two modes for CI/CD:

| Mode | Duration | Use Case |
|------|----------|----------|
| **Light** | ~2-3 minutes | PR validation - quick smoke tests |
| **Full** | ~15-20 minutes | Main branch - comprehensive analysis |

## GitHub Actions

### Quick Setup

The repository includes a ready-to-use workflow at `.github/workflows/benchmark.yml`.

Features:
- ✅ Runs on every PR to `main`/`master`
- ✅ Posts benchmark results as PR comments
- ✅ Compares PR against main branch
- ✅ Fails build on significant regressions
- ✅ Stores historical benchmark data

### Workflow Triggers

```yaml
on:
  pull_request:
    branches: [ main, master ]
  push:
    branches: [ main, master ]
  workflow_dispatch:
    inputs:
      benchmark_all:
        description: 'Run full benchmark suite'
        default: 'false'
```

### Jobs

#### 1. `benchmark-pr` (Lightweight)
Runs on every PR:
- Memory footprint check
- Execution throughput (100 actions)
- Action cache performance (1000 ops)
- Cold start measurement
- Connection churn (100 connections)

#### 2. `benchmark-full` (Comprehensive)
Runs on main branch or manual trigger:
- Full 8-dimension benchmark suite
- Historical trend tracking
- Automated alerting on regressions

#### 3. `benchmark-compare`
Compares PR against main branch:
- Side-by-side performance comparison
- Memory usage delta
- Latency comparisons

#### 4. `regression-check`
Validates against thresholds:
- Memory: <20MB idle
- Cold start: <500ms
- Cleanup rate: >95%

## GitLab CI

For GitLab, add to `.gitlab-ci.yml`:

```yaml
stages:
  - build
  - benchmark

variables:
  CARGO_TERM_COLOR: always

build:
  stage: build
  image: rust:1.85
  script:
    - apt-get update && apt-get install -y protobuf-compiler
    - cargo build --release --bin rbe-server
  artifacts:
    paths:
      - target/release/rbe-server
    expire_in: 1 hour

benchmark:light:
  stage: benchmark
  image: python:3.11
  dependencies:
    - build
  script:
    - pip install grpcio grpcio-tools
    - cd benchmark && ./scripts/benchmark-ci.sh light
  artifacts:
    paths:
      - benchmark/results/
    reports:
      junit: benchmark/results/junit.xml
  only:
    - merge_requests

benchmark:full:
  stage: benchmark
  image: python:3.11
  dependencies:
    - build
  script:
    - pip install grpcio grpcio-tools
    - cd benchmark && ./scripts/benchmark-ci.sh full
  artifacts:
    paths:
      - benchmark/results/
  only:
    - main
    - master
```

## Local CI Testing

Test the CI pipeline locally before pushing:

```bash
# Build release binary
cargo build --release --bin rbe-server

# Run lightweight CI benchmark
cd benchmark
./scripts/benchmark-ci.sh light

# Check results
cat results/benchmark_summary.md
```

## Regression Thresholds

The CI system checks these thresholds:

| Metric | Threshold | Rationale |
|--------|-----------|-----------|
| `memory_mb` | < 20 MB | FerrisRBE should maintain low baseline |
| `cold_start_ms` | < 500 ms | Native binary should start quickly |
| `execution_p99_ms` | < 100 ms | Consistent execution latency |
| `cache_p99_us` | < 1000 μs | Fast action cache reads |
| `churn_cleanup_rate` | > 95% | Proper resource cleanup |
| `streaming_delta_mb` | < 100 MB | O(1) streaming verified |

To modify thresholds, edit `scripts/check-regression.py`:

```python
THRESHOLDS = {
    "memory_mb": Threshold("memory_mb", 20.0, "MB"),
    "cold_start_ms": Threshold("cold_start_ms", 500.0, "ms"),
    # ... add or modify thresholds
}
```

## Understanding CI Results

### PR Comment Format

The bot posts a comment like this:

```markdown
## 🚀 Performance Benchmark Results

**Mode:** light  
**Timestamp:** 2024-01-15 10:30:45 UTC

#### Memory Footprint
- **Idle Memory:** 6.7 MB
✅ Memory usage within expected range

#### Cold Start Time
- **Startup Time:** 45ms
✅ Cold start within expected range

#### Detailed Results
- execution_20240115_103045: ✅ Completed
- cache_20240115_103045: ✅ Completed
```

### Comparison Comment

When comparing against main:

```markdown
## 📊 Performance Comparison: PR vs Main

| Metric | Main | PR | Change | Status |
|--------|------|-----|--------|--------|
| Memory (MB) | 6.5 | 6.7 | ↑ 3.0% | ✅ |

#### Legend
- ✅ Within 5% - Acceptable
- ⚠️ 5-15% change - Review recommended
- 🚨 >15% regression - Optimization required
```

## Handling Regressions

### If CI Fails Due to Regression

1. **Check the benchmark results** in the PR comment
2. **Identify the specific metric** that regressed
3. **Reproduce locally**:
   ```bash
   cd benchmark
   ./scripts/benchmark.sh
   ./scripts/check-regression.py results/benchmark_data.json
   ```
4. **Optimize** the code causing regression
5. **Re-run CI** after fixes

### Common Causes of Regressions

| Symptom | Likely Cause | Solution |
|---------|--------------|----------|
| Memory increase | New allocations | Use `Box::leak` check, review `clone()` calls |
| Cold start increase | New dependencies | Check `lazy_static` usage, deferred init |
| Latency increase | Lock contention | Review `Mutex` usage, consider `DashMap` |
| Cleanup rate drop | Missing drop handlers | Ensure `Drop` implementations, check channels |

## Advanced Configuration

### Custom Benchmark Duration

For longer-running benchmarks:

```yaml
- name: Run extended benchmark
  run: |
    cd benchmark
    ./scripts/benchmark-ci.sh full
  env:
    BENCHMARK_DURATION: 300  # 5 minutes per test
```

### Selective Benchmarks

Run only specific benchmarks:

```bash
# Only memory and cold start
./scripts/benchmark-ci.sh light --tests memory,cold_start

# Skip streaming tests (slow)
./scripts/benchmark-ci.sh full --skip streaming
```

### Baseline Comparison

Compare against a specific baseline:

```bash
python3 scripts/check-regression.py \
    results/benchmark_data.json \
    --baseline results/baseline.json \
    --output report.md
```

## Troubleshooting

### "Server failed to start"

- Check if port 9092 is available
- Verify binary exists: `ls target/release/rbe-server`
- Check server logs: `cat results/rbe-server.log`

### "Benchmark timeout"

- Increase timeout in workflow: `timeout-minutes: 30`
- Reduce load: edit `benchmark-ci.sh` to use fewer actions

### "No baseline for comparison"

- Baseline is generated on main branch pushes
- First PR won't have comparison (expected)
- Subsequent PRs will compare against latest main

## Best Practices

1. **Always review benchmark results** before merging
2. **Don't ignore warnings** - they indicate potential issues
3. **Document intentional changes** that affect performance
4. **Run full benchmarks** before major releases
5. **Monitor trends** over time using GitHub's benchmark graphs

## Support

For issues with CI/CD integration:
- Check the [troubleshooting guide](../docs/troubleshooting.md)
- Review [GitHub Actions logs](https://docs.github.com/en/actions/monitoring-and-troubleshooting-workflows)
- Open an issue with the "ci/cd" label
