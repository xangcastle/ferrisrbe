#!/bin/bash
# CI/CD Benchmark Script
# Runs a lightweight or full benchmark suite for continuous integration
# Works with any Bazel configuration (--symlink_prefix)

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCHMARK_DIR="$(dirname "$SCRIPT_DIR")"

# Source Bazel utilities
source "$SCRIPT_DIR/bazel-utils.sh"

PROJECT_ROOT="$(get_workspace_root "$BENCHMARK_DIR/..")"
RESULTS_DIR="$BENCHMARK_DIR/results"
MODE="${1:-light}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() { echo -e "${BLUE}[INFO]${NC} $1"; }
log_success() { echo -e "${GREEN}[SUCCESS]${NC} $1"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }
log_error() { echo -e "${RED}[ERROR]${NC} $1"; }

mkdir -p "$RESULTS_DIR"

# Generate timestamp
TIMESTAMP=$(date +%Y%m%d_%H%M%S)

cleanup() {
    log_info "Cleaning up..."
    # Stop supporting services first
    stop_services
    # Kill any lingering server processes
    pkill -f rbe-server 2>/dev/null || true
    rm -f "$RESULTS_DIR"/rbe-server.log 2>/dev/null || true
}

trap cleanup EXIT

# Build FerrisRBE for benchmarking using Bazel (dogfooding)
build() {
    log_info "Building FerrisRBE with Bazel..."
    
    cd "$PROJECT_ROOT"
    
    # Use Bazel to build (consistent with project philosophy)
    # This works regardless of --symlink_prefix setting
    local server_output=$(bazel_build_and_get_output "//:rbe-server" "$PROJECT_ROOT" "release")
    
    if [ -z "$server_output" ] || [ ! -f "$server_output" ]; then
        log_warn "Bazel build failed or output not found"
        log_info "Attempting fallback to cargo..."
        
        if command -v cargo &> /dev/null; then
            cargo build --release --bin rbe-server
            server_output="$PROJECT_ROOT/target/release/rbe-server"
        else
            log_error "Neither Bazel nor Cargo available. Cannot build."
            exit 1
        fi
    fi
    
    # Store output path for later use
    echo "$server_output" > "$PROJECT_ROOT/.bazel-output-server"
    
    log_success "Build complete: $server_output"
}

# Get server binary path (works with any symlink prefix)
get_server_binary() {
    # Check if we have a stored output path
    if [ -f "$PROJECT_ROOT/.bazel-output-server" ]; then
        local path=$(cat "$PROJECT_ROOT/.bazel-output-server")
        if [ -f "$path" ]; then
            echo "$path"
            return 0
        fi
    fi
    
    # Try to find using Bazel utils
    local path=$(find_bazel_output "//:rbe-server" "$PROJECT_ROOT")
    if [ -n "$path" ] && [ -f "$path" ]; then
        echo "$path"
        return 0
    fi
    
    # Fallback locations
    local fallbacks=(
        "$PROJECT_ROOT/target/release/rbe-server"
    )
    
    for path in "${fallbacks[@]}"; do
        if [ -f "$path" ]; then
            echo "$path"
            return 0
        fi
    done
    
    echo ""
    return 1
}

# Start supporting services (bazel-remote for CAS)
start_services() {
    log_info "Starting supporting services..."
    
    # Check if we're using GitHub Actions services
    if [ -n "$BENCHMARK_SERVICES" ]; then
        log_info "Using GitHub Actions services for bazel-remote..."
        # Wait for the service to be ready (GitHub Actions starts it automatically)
        for i in {1..60}; do
            if nc -z localhost 9094 2>/dev/null; then
                log_success "CAS (bazel-remote) is ready on port 9094"
                return 0
            fi
            log_info "Waiting for bazel-remote to be ready... (attempt $i/60)"
            sleep 2
        done
        log_warn "bazel-remote did not become ready in time"
        return 1
    fi
    
    # Check if we're in standalone mode (no external deps) - legacy mode
    if [ -n "$BENCHMARK_STANDALONE" ]; then
        log_warn "BENCHMARK_STANDALONE is deprecated. Use BENCHMARK_SERVICES with GitHub Actions."
        log_warn "Attempting to start bazel-remote locally..."
        
        # Try to find or download bazel-remote
        local bazel_remote_path=""
        if command -v bazel-remote &> /dev/null; then
            bazel_remote_path=$(which bazel-remote)
        elif [ -f "$PROJECT_ROOT/.cache/bazel-remote" ]; then
            bazel_remote_path="$PROJECT_ROOT/.cache/bazel-remote"
        else
            # Download bazel-remote binary
            log_info "Downloading bazel-remote..."
            mkdir -p "$PROJECT_ROOT/.cache"
            local bazel_remote_version="1.3.23"
            local download_url="https://github.com/buchgr/bazel-remote/releases/download/v${bazel_remote_version}/bazel-remote-${bazel_remote_version}-linux-x86_64"
            curl -sL -o "$PROJECT_ROOT/.cache/bazel-remote" "$download_url" || true
            chmod +x "$PROJECT_ROOT/.cache/bazel-remote" 2>/dev/null || true
            bazel_remote_path="$PROJECT_ROOT/.cache/bazel-remote"
        fi
        
        if [ -x "$bazel_remote_path" ]; then
            log_info "Starting bazel-remote on port 9094..."
            mkdir -p "$RESULTS_DIR/bazel-remote-cache"
            "$bazel_remote_path" --dir="$RESULTS_DIR/bazel-remote-cache" --port=9094 --grpc_port=9094 &
            BAZEL_REMOTE_PID=$!
            
            # Wait for bazel-remote to be ready
            for i in {1..30}; do
                if nc -z localhost 9094 2>/dev/null; then
                    log_success "bazel-remote ready (PID: $BAZEL_REMOTE_PID)"
                    break
                fi
                sleep 1
            done
        else
            log_warn "Could not find or download bazel-remote, CAS operations may fail"
        fi
    fi
    
    # Verify CAS is available if needed
    if nc -z localhost 9094 2>/dev/null; then
        log_success "CAS (bazel-remote) available on port 9094"
    else
        log_warn "CAS not available on port 9094, some tests may fail"
    fi
}

# Stop supporting services
stop_services() {
    if [ -n "$BAZEL_REMOTE_PID" ]; then
        log_info "Stopping bazel-remote (PID: $BAZEL_REMOTE_PID)..."
        kill $BAZEL_REMOTE_PID 2>/dev/null || true
        wait $BAZEL_REMOTE_PID 2>/dev/null || true
    fi
}

# Start FerrisRBE server
start_server() {
    log_info "Starting FerrisRBE server..."
    
    local server_binary=$(get_server_binary)
    
    if [ -z "$server_binary" ]; then
        log_error "rbe-server binary not found."
        log_info "Build with Bazel: bazel build //:rbe-server --config=release"
        log_info "Or with Cargo: cargo build --release --bin rbe-server"
        exit 1
    fi
    
    log_info "Using server binary: $server_binary"
    
    # Ensure CAS_ENDPOINT is set (for services mode, it's set by workflow)
    if [ -z "$CAS_ENDPOINT" ]; then
        export CAS_ENDPOINT="localhost:9094"
        log_info "CAS_ENDPOINT not set, using default: $CAS_ENDPOINT"
    else
        log_info "Using CAS_ENDPOINT: $CAS_ENDPOINT"
    fi
    
    "$server_binary" &
    SERVER_PID=$!
    
    # Wait for server to be ready
    log_info "Waiting for server to be ready..."
    for i in {1..60}; do
        if nc -z localhost 9092 2>/dev/null; then
            log_success "Server ready (PID: $SERVER_PID)"
            return 0
        fi
        sleep 1
    done
    
    log_error "Server failed to start within 60 seconds"
    exit 1
}

# Stop server
stop_server() {
    if [ -n "$SERVER_PID" ]; then
        log_info "Stopping server (PID: $SERVER_PID)..."
        kill $SERVER_PID 2>/dev/null || true
        wait $SERVER_PID 2>/dev/null || true
    fi
}

# Run memory benchmark
run_memory_benchmark() {
    log_info "Running memory footprint benchmark..."
    
    # Get memory usage via ps (works regardless of container/native)
    for i in {1..5}; do
        ps -o rss= -p $SERVER_PID 2>/dev/null | awk '{print $1/1024}' || echo "0"
        sleep 1
    done | tee "$RESULTS_DIR/memory_${TIMESTAMP}.txt"
    
    # Parse and store result (first non-zero value)
    MEMORY_MB=$(grep -v "^0$" "$RESULTS_DIR/memory_${TIMESTAMP}.txt" | head -1)
    if [ -z "$MEMORY_MB" ]; then
        MEMORY_MB="0"
    fi
    
    echo "$MEMORY_MB" > "$RESULTS_DIR/memory_baseline.txt"
    
    log_success "Memory baseline: ${MEMORY_MB}MB"
}

# Run lightweight benchmarks
run_light_benchmarks() {
    log_info "=== LIGHTWEIGHT BENCHMARK SUITE ==="
    log_info "Running quick tests suitable for PR validation..."
    
    # 1. Memory footprint (quick)
    log_info "Test 1/5: Memory footprint..."
    run_memory_benchmark
    
    # 2. Execution throughput (reduced load)
    log_info "Test 2/5: Execution throughput (light)..."
    python3 "$SCRIPT_DIR/execution-load-test.py" \
        --server localhost:9092 \
        --actions 100 \
        --concurrent 10 \
        --output "$RESULTS_DIR/execution_${TIMESTAMP}.json" || true
    
    # 3. Action cache (reduced operations)
    log_info "Test 3/5: Action cache performance (light)..."
    python3 "$SCRIPT_DIR/action-cache-test.py" \
        --server localhost:9092 \
        --operations 1000 \
        --concurrent 20 \
        --operation read \
        --output "$RESULTS_DIR/cache_${TIMESTAMP}.json" || true
    
    # 4. Cold start (single measurement)
    log_info "Test 4/5: Cold start time..."
    stop_server
    START_TIME=$(date +%s%N)
    start_server
    END_TIME=$(date +%s%N)
    COLD_START_MS=$(( (END_TIME - START_TIME) / 1000000 ))
    echo "$COLD_START_MS" > "$RESULTS_DIR/coldstart_${TIMESTAMP}.txt"
    log_success "Cold start: ${COLD_START_MS}ms"
    
    # 5. Connection churn (reduced)
    log_info "Test 5/5: Connection churn (light)..."
    python3 "$SCRIPT_DIR/connection-churn-test.py" \
        --server localhost:9092 \
        --connections 100 \
        --disconnect-rate 0.3 \
        --output "$RESULTS_DIR/churn_${TIMESTAMP}.json" || true
    
    log_success "Lightweight benchmarks complete!"
}

# Run full benchmarks
run_full_benchmarks() {
    log_info "=== FULL BENCHMARK SUITE ==="
    log_info "Running comprehensive tests (this will take time)..."
    
    # 1. Memory footprint (extended)
    log_info "Test 1/8: Memory footprint..."
    run_memory_benchmark
    
    # 2. Execution throughput
    log_info "Test 2/8: Execution throughput..."
    python3 "$SCRIPT_DIR/execution-load-test.py" \
        --server localhost:9092 \
        --actions 1000 \
        --concurrent 50 \
        --output "$RESULTS_DIR/execution_${TIMESTAMP}.json"
    
    # 3. Action cache
    log_info "Test 3/8: Action cache performance..."
    python3 "$SCRIPT_DIR/action-cache-test.py" \
        --server localhost:9092 \
        --operations 10000 \
        --concurrent 100 \
        --operation read \
        --output "$RESULTS_DIR/cache_${TIMESTAMP}.json"
    
    # 4. Noisy neighbor (scheduler fairness)
    log_info "Test 4/8: Scheduler fairness (noisy neighbor)..."
    python3 "$SCRIPT_DIR/noisy-neighbor-test.py" \
        --server localhost:9092 \
        --slow 10 \
        --fast 50 \
        --output "$RESULTS_DIR/scheduler_${TIMESTAMP}.json"
    
    # 5. O(1) Streaming (with smaller files for CI)
    log_info "Test 5/8: O(1) Streaming..."
    python3 "$SCRIPT_DIR/o1-streaming-test.py" \
        --server localhost:9092 \
        --large-sizes 1 \
        --small-count 100 \
        --container ferrisrbe-server \
        --output "$RESULTS_DIR/streaming_${TIMESTAMP}.json" || true
    
    # 6. Connection churn
    log_info "Test 6/8: Connection churn..."
    python3 "$SCRIPT_DIR/connection-churn-test.py" \
        --server localhost:9092 \
        --connections 1000 \
        --disconnect-rate 0.3 \
        --output "$RESULTS_DIR/churn_${TIMESTAMP}.json"
    
    # 7. Cache stampede
    log_info "Test 7/8: Cache stampede (thundering herd)..."
    python3 "$SCRIPT_DIR/cache-stampede-test.py" \
        --server localhost:9092 \
        --requests 10000 \
        --concurrent 100 \
        --output "$RESULTS_DIR/stampede_${TIMESTAMP}.json"
    
    # 8. CAS load test
    log_info "Test 8/8: CAS operations..."
    python3 "$SCRIPT_DIR/cas-load-test.py" \
        --server localhost:9092 \
        --blobs 100 \
        --size 1048576 \
        --concurrent 10 \
        --output "$RESULTS_DIR/cas_${TIMESTAMP}.json"
    
    log_success "Full benchmarks complete!"
}

# Generate summary report
generate_summary() {
    log_info "Generating summary report..."
    
    SUMMARY_FILE="$RESULTS_DIR/benchmark_summary.md"
    
    cat > "$SUMMARY_FILE" << EOF
### Benchmark Results Summary

**Mode:** ${MODE}  
**Timestamp:** $(date -u +"%Y-%m-%d %H:%M:%S UTC")  
**Commit:** ${GITHUB_SHA:-N/A}

EOF
    
    # Memory results
    if [ -f "$RESULTS_DIR/memory_baseline.txt" ]; then
        MEMORY=$(cat "$RESULTS_DIR/memory_baseline.txt")
        echo "#### Memory Footprint" >> "$SUMMARY_FILE"
        echo "- **Idle Memory:** ${MEMORY} MB" >> "$SUMMARY_FILE"
        echo "" >> "$SUMMARY_FILE"
        
        # Threshold check
        if (( $(echo "$MEMORY > 20" | bc -l) )); then
            echo "⚠️ **WARNING:** Memory usage (${MEMORY}MB) exceeds threshold (20MB)" >> "$SUMMARY_FILE"
        else
            echo "✅ Memory usage within expected range" >> "$SUMMARY_FILE"
        fi
        echo "" >> "$SUMMARY_FILE"
    fi
    
    # Cold start results
    if [ -f "$RESULTS_DIR/coldstart_${TIMESTAMP}.txt" ]; then
        COLD_START=$(cat "$RESULTS_DIR/coldstart_${TIMESTAMP}.txt")
        echo "#### Cold Start Time" >> "$SUMMARY_FILE"
        echo "- **Startup Time:** ${COLD_START}ms" >> "$SUMMARY_FILE"
        echo "" >> "$SUMMARY_FILE"
        
        if [ "$COLD_START" -gt 500 ]; then
            echo "⚠️ **WARNING:** Cold start (${COLD_START}ms) exceeds threshold (500ms)" >> "$SUMMARY_FILE"
        else
            echo "✅ Cold start within expected range" >> "$SUMMARY_FILE"
        fi
        echo "" >> "$SUMMARY_FILE"
    fi
    
    # JSON results summary
    echo "#### Detailed Results" >> "$SUMMARY_FILE"
    echo "" >> "$SUMMARY_FILE"
    
    for json in "$RESULTS_DIR"/*.json; do
        if [ -f "$json" ]; then
            BASENAME=$(basename "$json" .json)
            echo "- ${BASENAME}: ✅ Completed" >> "$SUMMARY_FILE"
        fi
    done
    
    echo "" >> "$SUMMARY_FILE"
    echo "---" >> "$SUMMARY_FILE"
    echo "*Generated by FerrisRBE Benchmark Suite*" >> "$SUMMARY_FILE"
    
    # Also generate JSON for programmatic access
    cat > "$RESULTS_DIR/benchmark_data.json" << EOF
{
    "timestamp": "$TIMESTAMP",
    "mode": "$MODE",
    "commit": "${GITHUB_SHA:-N/A}",
    "results": {
        "memory_mb": $(cat "$RESULTS_DIR/memory_baseline.txt" 2>/dev/null || echo "null"),
        "cold_start_ms": $(cat "$RESULTS_DIR/coldstart_${TIMESTAMP}.txt" 2>/dev/null || echo "null")
    }
}
EOF
    
    log_success "Summary generated: $SUMMARY_FILE"
    
    # Display summary
    cat "$SUMMARY_FILE"
}

# Main execution
main() {
    log_info "Starting CI Benchmark Suite"
    log_info "Mode: $MODE"
    log_info "Results directory: $RESULTS_DIR"
    
    if [ -n "$BENCHMARK_STANDALONE" ]; then
        log_info "Standalone mode enabled (starting local services)"
    fi
    
    # Start supporting services first
    start_services
    
    # Start server
    start_server
    
    # Run appropriate benchmark suite
    case "$MODE" in
        light)
            run_light_benchmarks
            ;;
        full)
            run_full_benchmarks
            ;;
        *)
            log_error "Unknown mode: $MODE. Use 'light' or 'full'"
            exit 1
            ;;
    esac
    
    # Generate summary
    generate_summary
    
    log_success "Benchmark suite complete!"
}

main "$@"
