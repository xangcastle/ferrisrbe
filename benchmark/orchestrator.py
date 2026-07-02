#!/usr/bin/env python3
"""Container-native benchmark orchestrator for FerrisRBE.

This script replaces the old Bash orchestration scripts
(`benchmark-ci.sh`, `benchmark-local.sh`, `compare-branches.sh`,
`generate-report.sh`, etc.) with a single Bazel-runnable entry point.

Usage:
    bazel run //benchmark:run -- light
    bazel run //benchmark:run -- full
    bazel run //benchmark:run -- compare --image ferrisrbe/server:latest
    bazel run //benchmark:run -- report
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Dict, List, Optional, Tuple

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

WORKSPACE_ROOT = Path(__file__).resolve().parents[1]
BENCHMARK_DIR = WORKSPACE_ROOT / "benchmark"
RESULTS_DIR = BENCHMARK_DIR / "results"

DEFAULT_SERVER_IMAGE = os.environ.get("BENCHMARK_SERVER_IMAGE", "ferrisrbe/server:latest")
DEFAULT_WORKER_IMAGE = os.environ.get("BENCHMARK_WORKER_IMAGE", "ferrisrbe/worker:latest")
DEFAULT_CACHE_IMAGE = os.environ.get("BENCHMARK_CACHE_IMAGE", "xangcastle/ferris-cache:latest")
OFFICIAL_SERVER_IMAGE = os.environ.get("BENCHMARK_OFFICIAL_IMAGE", "xangcastle/ferris-server:latest")

CAS_ENDPOINT = os.environ.get("CAS_ENDPOINT", "localhost:9094")
WORKER_MEMORY_LIMIT = os.environ.get("WORKER_MEMORY_LIMIT", "512m")
SERVER_MEMORY_LIMIT = os.environ.get("SERVER_MEMORY_LIMIT", "512m")

CONTAINER_PREFIX = "ferrisrbe-benchmark"
NETWORK_NAME = "ferrisrbe-benchmark-net"

LIGHT_TESTS = {
    "memory": True,
    "execution": {"actions": 100, "concurrent": 10},
    "action_cache": {"operations": 1000, "concurrent": 20},
    "cold_start": True,
    "connection_churn": {"connections": 100, "disconnect_rate": 0.3},
}

FULL_TESTS = {
    "memory": True,
    "execution": {"actions": 1000, "concurrent": 50},
    "action_cache": {"operations": 10000, "concurrent": 100},
    "noisy_neighbor": {"slow": 10, "fast": 50},
    "o1_streaming": {"large_sizes": [1], "small_count": 100},
    "connection_churn": {"connections": 1000, "disconnect_rate": 0.3},
    "cache_stampede": {"requests": 10000, "concurrent": 100},
    "cas_load": {"blobs": 100, "size": 1048576, "concurrent": 10},
    "cold_start": True,
}


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def log_info(msg: str) -> None:
    print(f"[INFO] {msg}")


def log_success(msg: str) -> None:
    print(f"[SUCCESS] {msg}")


def log_warn(msg: str) -> None:
    print(f"[WARN] {msg}", file=sys.stderr)


def log_error(msg: str) -> None:
    print(f"[ERROR] {msg}", file=sys.stderr)


def run(cmd: List[str], check: bool = True, cwd: Optional[Path] = None, **kwargs) -> subprocess.CompletedProcess:
    """Run a shell command and return the result."""
    log_info("$ " + " ".join(cmd))
    return subprocess.run(
        cmd,
        cwd=str(cwd or WORKSPACE_ROOT),
        check=check,
        text=True,
        capture_output=True,
        **kwargs,
    )


def container_runtime_available() -> bool:
    return shutil.which("podman") is not None


def container_exists(name: str) -> bool:
    result = run(["podman", "ps", "-aq", "--filter", f"name={name}"], check=False)
    return bool(result.stdout.strip())


def stop_container(name: Optional[str]) -> None:
    if not name:
        return
    if container_exists(name):
        run(["podman", "rm", "-f", name], check=False)


def list_benchmark_containers() -> List[str]:
    """Return names of all containers created by this orchestrator."""
    result = run(
        ["podman", "ps", "-aq", "--format", "{{.Names}}"],
        check=False,
    )
    if result.returncode != 0 or not result.stdout.strip():
        return []
    names = []
    for line in result.stdout.strip().splitlines():
        name = line.strip()
        if name.startswith(CONTAINER_PREFIX) or name == "rbe-cache":
            names.append(name)
    return names


def cleanup_all_benchmark_containers() -> None:
    """Force-remove any benchmark containers left over from previous runs."""
    names = list_benchmark_containers()
    if not names:
        return
    log_info(f"Removing {len(names)} leftover benchmark container(s): {', '.join(names)}")
    for name in names:
        run(["podman", "rm", "-f", name], check=False)
    # Give the host/port mapper a moment to release bindings.
    time.sleep(1)


def ensure_network() -> None:
    """Create the bridge network used by benchmark containers."""
    result = run(["podman", "network", "inspect", NETWORK_NAME], check=False)
    if result.returncode != 0:
        log_info(f"Creating container network: {NETWORK_NAME}")
        run(["podman", "network", "create", NETWORK_NAME], check=False)


def remove_network() -> None:
    """Remove the bridge network if it exists."""
    result = run(["podman", "network", "inspect", NETWORK_NAME], check=False)
    if result.returncode == 0:
        log_info(f"Removing container network: {NETWORK_NAME}")
        run(["podman", "network", "rm", NETWORK_NAME], check=False)


def wait_for_port(port: int, timeout: int = 60) -> bool:
    """Wait until a TCP port accepts connections."""
    start = time.time()
    while time.time() - start < timeout:
        try:
            import socket
            with socket.create_connection(("localhost", port), timeout=1):
                return True
        except OSError:
            time.sleep(0.5)
    return False


def wait_for_port_free(port: int, timeout: int = 30) -> bool:
    """Wait until a TCP port is no longer accepting connections."""
    start = time.time()
    while time.time() - start < timeout:
        try:
            import socket
            with socket.create_connection(("localhost", port), timeout=1):
                time.sleep(0.5)
        except OSError:
            return True
    return False


def get_container_memory_mb(name: str) -> float:
    """Return the latest memory usage in MB for a container."""
    result = run(
        ["podman", "stats", name, "--no-stream", "--format", "{{.MemUsage}}"],
        check=False,
    )
    if result.returncode != 0 or not result.stdout.strip():
        return 0.0
    # Format is usually "6.5MiB / 512MiB" or similar.
    raw = result.stdout.strip().split()[0]
    value = float("".join(c for c in raw if c.isdigit() or c == "."))
    if "GiB" in raw:
        value *= 1024
    elif "kB" in raw:
        value /= 1024
    elif "MB" in raw:
        pass  # Already in megabytes.
    elif "MiB" in raw:
        pass  # Treat as megabytes for reporting.
    return value


def get_image_info(image: str) -> Dict[str, str]:
    result = run(["podman", "images", "--format", "{{.ID}}|{{.Size}}|{{.CreatedAt}}", image], check=False)
    if result.returncode != 0 or not result.stdout.strip():
        return {"id": "N/A", "size": "N/A", "created": "N/A"}
    parts = result.stdout.strip().split("|")
    return {
        "id": parts[0] if len(parts) > 0 else "N/A",
        "size": parts[1] if len(parts) > 1 else "N/A",
        "created": parts[2] if len(parts) > 2 else "N/A",
    }


def bazel_run_target(target: str, args: List[str]) -> subprocess.CompletedProcess:
    """Run a Bazel target and stream output."""
    cmd = ["bazel", "run", target, "--"] + args
    log_info("$ " + " ".join(cmd))
    return subprocess.run(cmd, cwd=str(WORKSPACE_ROOT), text=True, check=False)


# ---------------------------------------------------------------------------
# Container lifecycle
# ---------------------------------------------------------------------------

class BenchmarkEnvironment:
    """Manages the rbe-cache, server and worker containers for a benchmark run."""

    def __init__(self, timestamp: str, server_image: str, worker_image: str, cache_image: str):
        self.timestamp = timestamp
        self.server_image = server_image
        self.worker_image = worker_image
        self.cache_image = cache_image
        self.server_name = f"{CONTAINER_PREFIX}-server-{timestamp}"
        self.worker_name = f"{CONTAINER_PREFIX}-worker-{timestamp}"
        self.cache_name = "rbe-cache"
        self.cache_started_here = False
        self.results: Dict[str, any] = {}

    def ensure_cache(self) -> None:
        """Start or reuse an rbe-cache container."""
        if os.environ.get("BENCHMARK_SERVICES"):
            log_info("Using existing GitHub Actions service for rbe-cache")
            if not wait_for_port(9094, timeout=90):
                log_warn("rbe-cache service did not become ready")
            return

        if container_exists(self.cache_name):
            log_info("Reusing existing rbe-cache container")
            if wait_for_port(9094, timeout=30):
                return
            log_warn("Existing rbe-cache not responding, removing it")
            stop_container(self.cache_name)

        log_info(f"Starting rbe-cache container from {self.cache_image}")
        RESULTS_DIR.mkdir(parents=True, exist_ok=True)
        cache_data = RESULTS_DIR / "rbe-cache-data"
        cache_data.mkdir(parents=True, exist_ok=True)

        ensure_network()
        run([
            "podman", "run", "-d",
            "--name", self.cache_name,
            "--network", NETWORK_NAME,
            "-p", "9094:9094",
            "-p", "8080:8080",
            "-v", f"{cache_data}:/data",
            "-e", "RUST_LOG=info",
            "-e", "RBE_CACHE_PORT=9094",
            "-e", "RBE_CACHE_HTTP_PORT=8080",
            "-e", "RBE_CACHE_DIR=/data",
            "-e", "RBE_CACHE_MAX_SIZE_GB=1",
            "-e", "RBE_CACHE_AC_MAX_SIZE_GB=1",
            self.cache_image,
        ])
        self.cache_started_here = True

        if not wait_for_port(9094, timeout=60):
            log_error("rbe-cache did not become ready")
            raise RuntimeError("rbe-cache failed to start")
        log_success("rbe-cache ready")

    def start_server(self) -> None:
        log_info(f"Starting server container from {self.server_image}")
        ensure_network()
        run([
            "podman", "run", "-d",
            "--name", self.server_name,
            "--network", NETWORK_NAME,
            "-p", "9092:9092",
            "-e", "RBE_PORT=9092",
            "-e", "RBE_BIND_ADDRESS=0.0.0.0",
            "-e", "RUST_LOG=info",
            "-e", f"CAS_ENDPOINT={self.cache_name}:9094",
            "-e", "RBE_L1_CACHE_CAPACITY=100000",
            "-e", "RBE_L1_CACHE_TTL_SECS=3600",
            "--memory", SERVER_MEMORY_LIMIT,
            self.server_image,
        ])

        if not wait_for_port(9092, timeout=60):
            logs = run(["podman", "logs", self.server_name], check=False).stdout
            log_error("Server failed to start")
            print(logs[-2000:] if logs else "(no logs)", file=sys.stderr)
            raise RuntimeError("Server failed to start")
        log_success("Server ready")

    def start_worker(self) -> None:
        log_info(f"Starting worker container from {self.worker_image}")
        worker_id = f"benchmark-worker-{self.timestamp}"
        ensure_network()
        run([
            "podman", "run", "-d",
            "--name", self.worker_name,
            "--network", NETWORK_NAME,
            "-e", f"WORKER_ID={worker_id}",
            "-e", f"SERVER_ENDPOINT=http://{self.server_name}:9092",
            "-e", f"CAS_ENDPOINT={self.cache_name}:9094",
            "-e", "RUST_LOG=info",
            "-e", "MAX_CONCURRENT=4",
            "--memory", WORKER_MEMORY_LIMIT,
            self.worker_image,
        ])
        log_success("Worker container started")

    def cleanup(self) -> None:
        log_info("Cleaning up benchmark containers...")
        # Remove the containers we created plus any leftovers from prior runs.
        cleanup_all_benchmark_containers()
        remove_network()
        log_success("Cleanup complete")


# ---------------------------------------------------------------------------
# Benchmark execution
# ---------------------------------------------------------------------------

def run_benchmark_script(name: str, args: List[str]) -> bool:
    target = f"//benchmark/scripts:{name}"
    result = bazel_run_target(target, args)
    return result.returncode == 0


def measure_memory(env: BenchmarkEnvironment) -> float:
    log_info("Measuring idle server memory...")
    samples = []
    for _ in range(5):
        mb = get_container_memory_mb(env.server_name)
        if mb > 0:
            samples.append(mb)
        time.sleep(1)
    if not samples:
        return 0.0
    return min(samples)


def run_light_benchmarks(env: BenchmarkEnvironment) -> Dict[str, any]:
    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    results: Dict[str, any] = {}

    if LIGHT_TESTS.get("memory"):
        results["memory_mb"] = measure_memory(env)
        (RESULTS_DIR / "memory_baseline.txt").write_text(str(results["memory_mb"]))

    env.start_worker()
    time.sleep(2)

    exec_cfg = LIGHT_TESTS["execution"]
    if run_benchmark_script("execution_load_test", [
        "--server", "localhost:9092",
        "--actions", str(exec_cfg["actions"]),
        "--concurrent", str(exec_cfg["concurrent"]),
        "--output", str(RESULTS_DIR / f"execution_{env.timestamp}.json"),
    ]):
        results["execution"] = "completed"

    ac_cfg = LIGHT_TESTS["action_cache"]
    if run_benchmark_script("action_cache_test", [
        "--server", "localhost:9092",
        "--operations", str(ac_cfg["operations"]),
        "--concurrent", str(ac_cfg["concurrent"]),
        "--operation", "read",
        "--output", str(RESULTS_DIR / f"cache_{env.timestamp}.json"),
    ]):
        results["action_cache"] = "completed"

    churn_cfg = LIGHT_TESTS["connection_churn"]
    if run_benchmark_script("connection_churn_test", [
        "--server", "localhost:9092",
        "--connections", str(churn_cfg["connections"]),
        "--disconnect-rate", str(churn_cfg["disconnect_rate"]),
        "--output", str(RESULTS_DIR / f"churn_{env.timestamp}.json"),
    ]):
        results["connection_churn"] = "completed"

    if LIGHT_TESTS.get("cold_start"):
        results["cold_start_ms"] = measure_cold_start(env.server_image, env.server_name, env.cache_name)

    return results


def run_full_benchmarks(env: BenchmarkEnvironment) -> Dict[str, any]:
    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    results: Dict[str, any] = {}

    if FULL_TESTS.get("memory"):
        results["memory_mb"] = measure_memory(env)
        (RESULTS_DIR / "memory_baseline.txt").write_text(str(results["memory_mb"]))

    env.start_worker()
    time.sleep(2)

    exec_cfg = FULL_TESTS["execution"]
    if run_benchmark_script("execution_load_test", [
        "--server", "localhost:9092",
        "--actions", str(exec_cfg["actions"]),
        "--concurrent", str(exec_cfg["concurrent"]),
        "--output", str(RESULTS_DIR / f"execution_{env.timestamp}.json"),
    ]):
        results["execution"] = "completed"

    ac_cfg = FULL_TESTS["action_cache"]
    if run_benchmark_script("action_cache_test", [
        "--server", "localhost:9092",
        "--operations", str(ac_cfg["operations"]),
        "--concurrent", str(ac_cfg["concurrent"]),
        "--operation", "read",
        "--output", str(RESULTS_DIR / f"cache_{env.timestamp}.json"),
    ]):
        results["action_cache"] = "completed"

    nn_cfg = FULL_TESTS.get("noisy_neighbor")
    if nn_cfg and run_benchmark_script("noisy_neighbor_test", [
        "--server", "localhost:9092",
        "--slow", str(nn_cfg["slow"]),
        "--fast", str(nn_cfg["fast"]),
        "--output", str(RESULTS_DIR / f"scheduler_{env.timestamp}.json"),
    ]):
        results["noisy_neighbor"] = "completed"

    stream_cfg = FULL_TESTS.get("o1_streaming")
    if stream_cfg and run_benchmark_script("o1_streaming_test", [
        "--server", "localhost:9092",
        "--large-sizes", " ".join(str(s) for s in stream_cfg["large_sizes"]),
        "--small-count", str(stream_cfg["small_count"]),
        "--container", env.server_name,
        "--output", str(RESULTS_DIR / f"streaming_{env.timestamp}.json"),
    ]):
        results["o1_streaming"] = "completed"

    churn_cfg = FULL_TESTS["connection_churn"]
    if run_benchmark_script("connection_churn_test", [
        "--server", "localhost:9092",
        "--connections", str(churn_cfg["connections"]),
        "--disconnect-rate", str(churn_cfg["disconnect_rate"]),
        "--output", str(RESULTS_DIR / f"churn_{env.timestamp}.json"),
    ]):
        results["connection_churn"] = "completed"

    stampede_cfg = FULL_TESTS.get("cache_stampede")
    if stampede_cfg and run_benchmark_script("cache_stampede_test", [
        "--server", "localhost:9092",
        "--requests", str(stampede_cfg["requests"]),
        "--concurrent", str(stampede_cfg["concurrent"]),
        "--output", str(RESULTS_DIR / f"stampede_{env.timestamp}.json"),
    ]):
        results["cache_stampede"] = "completed"

    cas_cfg = FULL_TESTS.get("cas_load")
    if cas_cfg and run_benchmark_script("cas_load_test", [
        "--server", "localhost:9092",
        "--blobs", str(cas_cfg["blobs"]),
        "--size", str(cas_cfg["size"]),
        "--concurrent", str(cas_cfg["concurrent"]),
        "--output", str(RESULTS_DIR / f"cas_{env.timestamp}.json"),
    ]):
        results["cas_load"] = "completed"

    if FULL_TESTS.get("cold_start"):
        results["cold_start_ms"] = measure_cold_start(env.server_image, env.server_name, env.cache_name)

    return results


def measure_cold_start(image: str, current_server_name: str, cache_name: str = "rbe-cache") -> Optional[int]:
    """Stop the current server, start a fresh one and measure ready time."""
    log_info("Measuring container cold start...")
    stop_container(current_server_name)
    if not wait_for_port_free(9092, timeout=30):
        log_warn("Port 9092 did not become free; forcing cleanup of all benchmark containers")
        cleanup_all_benchmark_containers()

    ensure_network()
    cold_name = f"{CONTAINER_PREFIX}-coldstart-{int(time.time())}"
    start = time.time_ns()
    run([
        "podman", "run", "-d",
        "--name", cold_name,
        "--network", NETWORK_NAME,
        "-p", "9092:9092",
        "-e", "RBE_PORT=9092",
        "-e", "RBE_BIND_ADDRESS=0.0.0.0",
        "-e", "RUST_LOG=info",
        "-e", f"CAS_ENDPOINT={cache_name}:9094",
        image,
    ])

    ready = wait_for_port(9092, timeout=60)
    elapsed_ms = (time.time_ns() - start) // 1_000_000
    if ready:
        log_success(f"Cold start: {elapsed_ms}ms")
    else:
        log_error("Cold start measurement failed: server did not become ready")
        elapsed_ms = 0

    stop_container(cold_name)
    (RESULTS_DIR / f"coldstart_{int(time.time())}.txt").write_text(str(elapsed_ms))
    return elapsed_ms


# ---------------------------------------------------------------------------
# Reports
# ---------------------------------------------------------------------------

def generate_summary(mode: str, timestamp: str, results: Dict[str, any], image: str) -> None:
    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    image_info = get_image_info(image)

    summary_path = RESULTS_DIR / "benchmark_summary.md"
    summary_lines = [
        f"### Benchmark Results Summary (Container Mode)",
        "",
        f"**Mode:** {mode}  ",
        f"**Timestamp:** {datetime.now(timezone.utc).strftime('%Y-%m-%d %H:%M:%S UTC')}  ",
        f"**Commit:** {os.environ.get('GITHUB_SHA', 'N/A')}  ",
        f"**Image:** {image}  ",
        f"**Image ID:** {image_info.get('id', 'N/A')}  ",
        f"**Image Size:** {image_info.get('size', 'N/A')}",
        "",
    ]

    memory_mb = results.get("memory_mb")
    if memory_mb is not None:
        summary_lines.extend([
            "#### Memory Footprint (Container)",
            f"- **Idle Memory:** {memory_mb:.1f} MB",
            "",
        ])
        if memory_mb > 20:
            summary_lines.append(f"⚠️ **WARNING:** Memory usage ({memory_mb:.1f}MB) exceeds threshold (20MB)\n")
        else:
            summary_lines.append("✅ Memory usage within expected range\n")

    cold_start_ms = results.get("cold_start_ms")
    if cold_start_ms is not None:
        summary_lines.extend([
            "#### Cold Start Time (Container)",
            f"- **Startup Time:** {cold_start_ms}ms",
            "",
        ])
        if cold_start_ms > 500:
            summary_lines.append(f"⚠️ **WARNING:** Cold start ({cold_start_ms}ms) exceeds threshold (500ms)\n")
        else:
            summary_lines.append("✅ Cold start within expected range\n")

    summary_lines.extend([
        "#### Detailed Results",
        "",
    ])
    for key, value in results.items():
        if key in ("memory_mb", "cold_start_ms"):
            continue
        summary_lines.append(f"- {key}: ✅ Completed")

    summary_lines.extend([
        "",
        "---",
        "*Generated by FerrisRBE Benchmark Suite (Bazel)*",
        "",
    ])

    summary_path.write_text("\n".join(summary_lines))
    log_success(f"Summary written to {summary_path}")

    data = {
        "timestamp": timestamp,
        "mode": mode,
        "commit": os.environ.get("GITHUB_SHA", "N/A"),
        "container": {
            "image": image,
            "image_id": image_info.get("id"),
            "image_size": image_info.get("size"),
        },
        "results": {
            "memory_mb": results.get("memory_mb"),
            "cold_start_ms": results.get("cold_start_ms"),
        },
    }
    (RESULTS_DIR / "benchmark_data.json").write_text(json.dumps(data, indent=2) + "\n")


def generate_report() -> None:
    """Generate a markdown report from existing JSON results."""
    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    json_files = sorted(RESULTS_DIR.glob("*.json"))
    if not json_files:
        log_warn("No JSON result files found")
        return

    lines = [
        "# FerrisRBE Benchmark Report",
        "",
        f"Generated: {datetime.now(timezone.utc).strftime('%Y-%m-%d %H:%M:%S UTC')}",
        "",
        "## Results",
        "",
    ]
    for path in json_files:
        try:
            data = json.loads(path.read_text())
        except json.JSONDecodeError:
            continue
        lines.append(f"### {path.stem}")
        lines.append("")
        lines.append("```json")
        lines.append(json.dumps(data, indent=2))
        lines.append("```")
        lines.append("")

    report_path = RESULTS_DIR / f"BENCHMARK_REPORT_{datetime.now(timezone.utc):%Y%m%d_%H%M%S}.md"
    report_path.write_text("\n".join(lines))
    log_success(f"Report written to {report_path}")


def compare_images(pr_image: str) -> None:
    """Compare the PR image against the official release image."""
    if not container_runtime_available():
        log_error("Podman is required for comparison")
        sys.exit(1)

    timestamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    official_container = f"ferrisrbe-official-{timestamp}"
    pr_container = f"ferrisrbe-pr-{timestamp}"

    log_info(f"Comparing PR image {pr_image} against {OFFICIAL_SERVER_IMAGE}")

    def benchmark_image(image: str, name: str, container_name: str) -> Tuple[Optional[float], Optional[Dict]]:
        log_info(f"Benchmarking {name}: {image}")
        run([
            "podman", "run", "-d",
            "--name", container_name,
            "--network", "host",
            "-p", "9092:9092",
            "-e", "RBE_PORT=9092",
            "-e", "RBE_BIND_ADDRESS=0.0.0.0",
            "-e", "RUST_LOG=info",
            "-e", f"CAS_ENDPOINT={CAS_ENDPOINT}",
            "--memory", SERVER_MEMORY_LIMIT,
            image,
        ])
        if not wait_for_port(9092, timeout=60):
            log_error(f"Server {name} failed to start")
            return None, None

        time.sleep(2)
        mem = get_container_memory_mb(container_name)
        info = get_image_info(image)

        # Quick throughput sample.
        run_benchmark_script("execution_load_test", [
            "--server", "localhost:9092",
            "--actions", "50",
            "--concurrent", "10",
            "--output", str(RESULTS_DIR / f"compare_{name}_{timestamp}.json"),
        ])

        stop_container(container_name)
        return mem, info

    try:
        run(["podman", "pull", OFFICIAL_SERVER_IMAGE], check=False)
        official_mem, official_info = benchmark_image(OFFICIAL_SERVER_IMAGE, "official", official_container)
        pr_mem, pr_info = benchmark_image(pr_image, "pr", pr_container)

        lines = [
            "## 📊 Performance Comparison: PR vs Official Release",
            "",
            "| Attribute | Official (latest) | PR |",
            "|-----------|-------------------|-----|",
            f"| **Image** | `{OFFICIAL_SERVER_IMAGE}` | `{pr_image}` |",
            f"| **Image ID** | `{official_info.get('id', 'N/A')[:12]}` | `{pr_info.get('id', 'N/A')[:12]}` |",
            f"| **Size** | {official_info.get('size', 'N/A')} | {pr_info.get('size', 'N/A')} |",
            f"| **Memory (MB)** | {official_mem if official_mem is not None else 'N/A'} | {pr_mem if pr_mem is not None else 'N/A'} |",
            "",
        ]

        if official_mem and pr_mem and official_mem > 0:
            change = ((pr_mem - official_mem) / official_mem) * 100
            arrow = "↓" if change < 0 else "↑" if change > 0 else "="
            status = "✅" if abs(change) <= 5 else "⚠️" if abs(change) <= 15 else "🚨"
            lines.append(f"**Memory change:** {arrow} {change:.1f}% {status}")
            lines.append("")

        lines.extend([
            "#### Legend",
            "- ✅ Within 5% - Acceptable",
            "- ⚠️ 5-15% change - Review recommended",
            "- 🚨 >15% regression - Optimization required",
            "",
        ])

        comparison_path = RESULTS_DIR / "comparison.md"
        comparison_path.write_text("\n".join(lines))
        log_success(f"Comparison written to {comparison_path}")
    finally:
        stop_container(official_container)
        stop_container(pr_container)


# ---------------------------------------------------------------------------
# Main entry points
# ---------------------------------------------------------------------------

def ensure_images(server_image: str, worker_image: str, cache_image: str) -> None:
    """Build or pull images if they are not present locally."""
    if not container_runtime_available():
        log_error("Podman is required for container-native benchmarks")
        sys.exit(1)

    for image, label in [(server_image, "server"), (worker_image, "worker"), (cache_image, "cache")]:
        result = run(["podman", "image", "inspect", image], check=False)
        if result.returncode == 0:
            log_info(f"Image present: {image}")
            continue

        if image.startswith("xangcastle/"):
            log_info(f"Pulling {label} image: {image}")
            run(["podman", "pull", image], check=False)
        else:
            log_info(f"Building {label} image via Bazel: {image}")
            arch = os.uname().machine
            arch_label = "arm64" if arch in ("arm64", "aarch64") else "amd64"
            target = f"//oci:{label}.{arch_label}.image.load"
            run(["bazel", "run", target], check=False)


def cmd_light(args: argparse.Namespace) -> int:
    timestamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    ensure_images(args.server_image, args.worker_image, args.cache_image)

    env = BenchmarkEnvironment(
        timestamp=timestamp,
        server_image=args.server_image,
        worker_image=args.worker_image,
        cache_image=args.cache_image,
    )
    cleanup_all_benchmark_containers()
    try:
        env.ensure_cache()
        env.start_server()
        results = run_light_benchmarks(env)
        generate_summary("light", timestamp, results, args.server_image)
        return 0
    finally:
        env.cleanup()


def cmd_full(args: argparse.Namespace) -> int:
    timestamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    ensure_images(args.server_image, args.worker_image, args.cache_image)

    env = BenchmarkEnvironment(
        timestamp=timestamp,
        server_image=args.server_image,
        worker_image=args.worker_image,
        cache_image=args.cache_image,
    )

    cleanup_all_benchmark_containers()
    try:
        env.ensure_cache()
        env.start_server()
        results = run_full_benchmarks(env)
        generate_summary("full", timestamp, results, args.server_image)
        return 0
    finally:
        env.cleanup()


def cmd_compare(args: argparse.Namespace) -> int:
    compare_images(args.image)
    return 0


def cmd_report(args: argparse.Namespace) -> int:
    generate_report()
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Bazel-native benchmark orchestrator for FerrisRBE",
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    for name, func in [("light", cmd_light), ("full", cmd_full), ("compare", cmd_compare), ("report", cmd_report)]:
        sub = subparsers.add_parser(name, help=f"Run {name} benchmark flow")
        sub.set_defaults(func=func)
        if name in ("light", "full"):
            sub.add_argument("--server-image", default=DEFAULT_SERVER_IMAGE)
            sub.add_argument("--worker-image", default=DEFAULT_WORKER_IMAGE)
            sub.add_argument("--cache-image", default=DEFAULT_CACHE_IMAGE)
        if name == "compare":
            sub.add_argument("--image", default=DEFAULT_SERVER_IMAGE)

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
