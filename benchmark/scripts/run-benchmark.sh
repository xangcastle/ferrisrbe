#!/bin/bash
# RBE Benchmark Runner
# Runs comparative benchmarks between FerrisRBE and other solutions

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCHMARK_DIR="$(dirname "$SCRIPT_DIR")"
PROJECT_DIR="$(dirname "$BENCHMARK_DIR")"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Default values
BLOBS=100
BLOB_SIZE=1048576  # 1MB
CONCURRENT=10
DURATION=300       # 5 minutes
RESULTS_DIR="$BENCHMARK_DIR/results"

# Print usage
usage() {
    cat <<EOF
RBE Benchmark Runner

Usage: $0 [OPTIONS] TARGET

TARGETS:
    ferrisrbe       Run benchmark against FerrisRBE
    buildfarm       Run benchmark against Bazel Buildfarm
    buildbarn       Run benchmark against Buildbarn
    buildbuddy      Run benchmark against BuildBuddy
    all             Run all benchmarks sequentially
    compare         Generate comparison report from existing results

OPTIONS:
    -b, --blobs NUM         Number of blobs to test (default: $BLOBS)
    -s, --size BYTES        Blob size in bytes (default: $BLOB_SIZE = 1MB)
    -c, --concurrent NUM    Concurrent operations (default: $CONCURRENT)
    -d, --duration SECS     Metrics collection duration (default: $DURATION)
    -r, --results DIR       Results directory (default: $RESULTS_DIR)
    -h, --help              Show this help

EXAMPLES:
    $0 ferrisrbe                    # Quick test with defaults
    $0 ferrisrbe -b 1000 -s 104857600   # 100 x 100MB blobs
    $0 all -d 600                   # Run all benchmarks for 10 minutes
    $0 compare                      # Generate comparison report

EOF
}

# Log functions
log_info() { echo -e "${BLUE}[INFO]${NC} $1"; }
log_success() { echo -e "${GREEN}[SUCCESS]${NC} $1"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }
log_error() { echo -e "${RED}[ERROR]${NC} $1"; }

# Parse arguments
parse_args() {
    while [[ $# -gt 0 ]]; do
        case $1 in
            -b|--blobs)
                BLOBS="$2"
                shift 2
                ;;
            -s|--size)
                BLOB_SIZE="$2"
                shift 2
                ;;
            -c|--concurrent)
                CONCURRENT="$2"
                shift 2
                ;;
            -d|--duration)
                DURATION="$2"
                shift 2
                ;;
            -r|--results)
                RESULTS_DIR="$2"
                shift 2
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            -*)
                log_error "Unknown option: $1"
                usage
                exit 1
                ;;
            *)
                TARGET="$1"
                shift
                ;;
        esac
    done

    if [[ -z "$TARGET" ]]; then
        log_error "No target specified"
        usage
        exit 1
    fi
}

# Setup environment
setup() {
    log_info "Setting up benchmark environment..."
    
    mkdir -p "$RESULTS_DIR"
    
    # Check dependencies
    command -v docker >/dev/null 2>&1 || { log_error "Docker is required but not installed"; exit 1; }
    command -v docker-compose >/dev/null 2>&1 || { log_error "Docker Compose is required"; exit 1; }
    
    # Create benchmark network if it doesn't exist
    docker network create benchmark-network 2>/dev/null || true
    
    log_success "Setup complete"
}

# Cleanup function
cleanup() {
    log_info "Cleaning up..."
    
    # Stop and remove containers for the current target
    case "$TARGET" in
        ferrisrbe)
            docker-compose -f "$BENCHMARK_DIR/docker-compose.ferrisrbe.yml" down -v 2>/dev/null || true
            ;;
        buildfarm)
            docker-compose -f "$BENCHMARK_DIR/docker-compose.buildfarm.yml" down -v 2>/dev/null || true
            ;;
        buildbarn)
            docker-compose -f "$BENCHMARK_DIR/docker-compose.buildbarn.yml" down -v 2>/dev/null || true
            ;;
        buildbuddy)
            docker-compose -f "$BENCHMARK_DIR/docker-compose.buildbuddy.yml" down -v 2>/dev/null || true
            ;;
    esac
    
    log_success "Cleanup complete"
}

# Wait for service to be healthy
wait_for_service() {
    local host="$1"
    local port="$2"
    local name="$3"
    local max_attempts=30
    local attempt=1
    
    log_info "Waiting for $name to be ready..."
    
    while [ $attempt -le $max_attempts ]; do
        if nc -z "$host" "$port" 2>/dev/null; then
            log_success "$name is ready"
            return 0
        fi
        echo -n "."
        sleep 2
        attempt=$((attempt + 1))
    done
    
    log_error "$name failed to start after $max_attempts attempts"
    return 1
}

# Run benchmark for a specific target
run_benchmark() {
    local target="$1"
    local compose_file="$BENCHMARK_DIR/docker-compose.$target.yml"
    local timestamp=$(date +%Y%m%d_%H%M%S)
    local result_dir="$RESULTS_DIR/${target}_${timestamp}"
    
    log_info "Starting benchmark for: $target"
    log_info "Results will be saved to: $result_dir"
    
    mkdir -p "$result_dir"
    
    # Save benchmark configuration
    cat > "$result_dir/config.json" <<EOF
{
    "target": "$target",
    "timestamp": "$timestamp",
    "blobs": $BLOBS,
    "blob_size": $BLOB_SIZE,
    "concurrent": $CONCURRENT,
    "duration": $DURATION
}
EOF
    
    # Start the RBE stack
    log_info "Starting $target stack..."
    docker-compose -f "$compose_file" up -d
    
    # Wait for services
    case "$target" in
        ferrisrbe)
            wait_for_service localhost 9092 "FerrisRBE Server"
            sleep 5  # Give extra time for worker registration
            ;;
        buildfarm)
            wait_for_service localhost 9092 "Buildfarm Server"
            sleep 10
            ;;
        buildbarn)
            wait_for_service localhost 9092 "Buildbarn Frontend"
            sleep 10
            ;;
        buildbuddy)
            wait_for_service localhost 9092 "BuildBuddy Server"
            wait_for_service localhost 8080 "BuildBuddy Web UI"
            sleep 10
            ;;
    esac
    
    # Get the server container name
    local server_container="${target}-server"
    [[ "$target" == "ferrisrbe" ]] && server_container="ferrisrbe-server"
    [[ "$target" == "buildbarn" ]] && server_container="buildbarn-frontend"
    [[ "$target" == "buildbuddy" ]] && server_container="buildbuddy-server"
    
    # Start metrics collection in background
    log_info "Starting metrics collection for ${DURATION}s..."
    python3 "$SCRIPT_DIR/metrics-collector.py" \
        --duration "$DURATION" \
        --interval 5 \
        --containers "$server_container" \
        --benchmark "$target" \
        --output "$result_dir/metrics.json" &
    local metrics_pid=$!
    
    # Wait a bit for metrics collection to start
    sleep 5
    
    # Run load test
    log_info "Running load test: $BLOBS blobs x $BLOB_SIZE bytes (concurrent: $CONCURRENT)"
    
    python3 "$SCRIPT_DIR/cas-load-test.py" \
        --server localhost:9092 \
        --blobs "$BLOBS" \
        --size "$BLOB_SIZE" \
        --concurrent "$CONCURRENT" \
        --output "$result_dir/loadtest.json" || {
        log_warn "Load test encountered errors (this may be expected for some configurations)"
    }
    
    # Wait for metrics collection to complete
    log_info "Waiting for metrics collection to complete..."
    wait $metrics_pid || true
    
    # Collect container info
    docker stats --no-stream "$server_container" > "$result_dir/docker_stats.txt" 2>/dev/null || true
    docker inspect "$server_container" > "$result_dir/container_inspect.json" 2>/dev/null || true
    
    # Generate quick summary
    log_info "Generating summary..."
    if [ -f "$result_dir/metrics.json" ]; then
        python3 <<EOF
import json
with open("$result_dir/metrics.json") as f:
    data = json.load(f)

print("\\n" + "="*60)
print(f"QUICK SUMMARY: {data['benchmark_name']}")
print("="*60)

for container, stats in data['summary'].items():
    mem = stats['memory']
    cpu = stats['cpu']
    print(f"\\n{container}:")
    print(f"  Memory: {mem['min_mb']:.1f} - {mem['max_mb']:.1f} MB (avg: {mem['avg_mb']:.1f})")
    print(f"  CPU: {cpu['min_percent']:.1f}% - {cpu['max_percent']:.1f}% (avg: {cpu['avg_percent']:.1f}%)")
EOF
    fi
    
    log_success "Benchmark complete for $target"
    log_info "Results saved to: $result_dir"
    
    # Cleanup
    log_info "Stopping $target stack..."
    docker-compose -f "$compose_file" down -v
    
    echo "$result_dir"
}

# Generate comparison report
generate_report() {
    log_info "Generating comparison report..."
    
    # Check if we have results
    if [ ! -d "$RESULTS_DIR" ] || [ -z "$(ls -A "$RESULTS_DIR")" ]; then
        log_error "No benchmark results found in $RESULTS_DIR"
        exit 1
    fi
    
    # Generate markdown report
    python3 <<EOF
import json
import os
import glob
from datetime import datetime

results_dir = "$RESULTS_DIR"
report_file = os.path.join(results_dir, "BENCHMARK_REPORT.md")

# Find all benchmark results
results = []
for metrics_file in glob.glob(f"{results_dir}/*/metrics.json"):
    with open(metrics_file) as f:
        data = json.load(f)
    
    config_file = metrics_file.replace("metrics.json", "config.json")
    config = {}
    if os.path.exists(config_file):
        with open(config_file) as f:
            config = json.load(f)
    
    # Extract key metrics
    for container, stats in data.get('summary', {}).items():
        if 'server' in container.lower() or 'frontend' in container.lower():
            mem_stats = stats['memory']
            cpu_stats = stats['cpu']
            
            results.append({
                'target': data['benchmark_name'],
                'container': container,
                'memory_min_mb': mem_stats['min_mb'],
                'memory_max_mb': mem_stats['max_mb'],
                'memory_avg_mb': mem_stats['avg_mb'],
                'cpu_avg_percent': cpu_stats['avg_percent'],
                'blobs': config.get('blobs', 'N/A'),
                'blob_size': config.get('blob_size', 'N/A'),
            })
            break

# Generate report
with open(report_file, 'w') as f:
    f.write("# RBE Benchmark Report\\n")
    f.write(f"Generated: {datetime.now().isoformat()}\\n\\n")
    
    f.write("## Summary\\n\\n")
    f.write("| Solution | Min Memory (MB) | Max Memory (MB) | Avg Memory (MB) | Avg CPU (%) |\\n")
    f.write("|----------|-----------------|-----------------|-----------------|-------------|\\n")
    
    for r in results:
        f.write(f"| {r['target']} | {r['memory_min_mb']:.1f} | {r['memory_max_mb']:.1f} | "
                f"{r['memory_avg_mb']:.1f} | {r['cpu_avg_percent']:.2f} |\\n")
    
    f.write("\\n## Detailed Results\\n\\n")
    for r in results:
        f.write(f"### {r['target']}\\n\\n")
        f.write(f"- **Test Parameters:** {r['blobs']} blobs x {r['blob_size']} bytes\\n")
        f.write(f"- **Container:** {r['container']}\\n")
        f.write(f"- **Memory Range:** {r['memory_min_mb']:.1f} - {r['memory_max_mb']:.1f} MB\\n")
        f.write(f"- **Average Memory:** {r['memory_avg_mb']:.1f} MB\\n")
        f.write(f"- **Average CPU:** {r['cpu_avg_percent']:.2f}%\\n\\n")
    
    f.write("## Raw Data\\n\\n")
    f.write("Full results are available in the following directories:\\n")
    for d in glob.glob(f"{results_dir}/*/"):
        f.write(f"- `{d}`\\n")

print(f"Report generated: {report_file}")
EOF

    log_success "Comparison report generated: $RESULTS_DIR/BENCHMARK_REPORT.md"
    cat "$RESULTS_DIR/BENCHMARK_REPORT.md"
}

# Main function
main() {
    parse_args "$@"
    
    # Setup trap for cleanup
    trap cleanup EXIT
    
    # Setup environment
    setup
    
    # Run benchmark(s)
    case "$TARGET" in
        ferrisrbe|buildfarm|buildbarn|buildbuddy)
            run_benchmark "$TARGET"
            ;;
        all)
            for target in ferrisrbe buildfarm buildbarn buildbuddy; do
                log_info "======================================="
                log_info "Running benchmark: $target"
                log_info "======================================="
                run_benchmark "$target" || log_warn "Benchmark for $target failed"
                sleep 10  # Cool down between benchmarks
            done
            generate_report
            ;;
        compare)
            generate_report
            ;;
        *)
            log_error "Unknown target: $TARGET"
            usage
            exit 1
            ;;
    esac
}

# Run main
main "$@"
