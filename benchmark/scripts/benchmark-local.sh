#!/bin/bash
# Local Benchmark Script
# Run benchmarks locally without CI dependencies
# Supports Docker for bazel-remote or local binary

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

# Check prerequisites
check_prerequisites() {
    log_info "Checking prerequisites..."
    
    local missing=()
    
    if ! command -v nc &> /dev/null; then
        missing+=("netcat (nc)")
    fi
    
    if ! command -v python3 &> /dev/null; then
        missing+=("python3")
    fi
    
    if [ ${#missing[@]} -gt 0 ]; then
        log_error "Missing prerequisites: ${missing[*]}"
        log_info "Install with:"
        log_info "  macOS: brew install netcat python3"
        log_info "  Linux: sudo apt-get install netcat-openbsd python3"
        exit 1
    fi
    
    log_success "Prerequisites OK"
}

# Start bazel-remote using Docker (preferred) or local binary
start_cas() {
    log_info "Starting CAS (bazel-remote)..."
    
    # Check if already running
    if nc -z localhost 9094 2>/dev/null; then
        log_success "CAS already running on port 9094"
        return 0
    fi
    
    # Try Docker first
    if command -v docker &> /dev/null; then
        log_info "Starting bazel-remote via Docker..."
        docker run -d \
            --name ferrisrbe-benchmark-cas \
            -p 9094:9094 \
            -p 8080:8080 \
            -v "$RESULTS_DIR/bazel-remote-cache:/data" \
            -e BAZEL_REMOTE_GRPC_PORT=9094 \
            -e BAZEL_REMOTE_HTTP_PORT=8080 \
            -e BAZEL_REMOTE_DIR=/data \
            -e BAZEL_REMOTE_MAX_SIZE=1 \
            buchgr/bazel-remote-cache:latest 2>/dev/null && {
            
            # Wait for it to be ready
            for i in {1..30}; do
                if nc -z localhost 9094 2>/dev/null; then
                    log_success "CAS ready (Docker)"
                    return 0
                fi
                sleep 1
            done
        }
        
        log_warn "Docker failed to start CAS, trying local binary..."
        docker rm -f ferrisrbe-benchmark-cas 2>/dev/null || true
    fi
    
    # Try local binary
    if command -v bazel-remote &> /dev/null; then
        log_info "Starting bazel-remote via local binary..."
        mkdir -p "$RESULTS_DIR/bazel-remote-cache"
        bazel-remote \
            --dir="$RESULTS_DIR/bazel-remote-cache" \
            --port=9094 \
            --grpc_port=9094 &
        BAZEL_REMOTE_PID=$!
        
        # Wait for it to be ready
        for i in {1..30}; do
            if nc -z localhost 9094 2>/dev/null; then
                log_success "CAS ready (local binary, PID: $BAZEL_REMOTE_PID)"
                return 0
            fi
            sleep 1
        done
        
        log_warn "Local binary failed to start CAS"
        kill $BAZEL_REMOTE_PID 2>/dev/null || true
    fi
    
    log_error "Could not start CAS. Please install one of:"
    log_error "  - Docker: https://docs.docker.com/get-docker/"
    log_error "  - bazel-remote: https://github.com/buchgr/bazel-remote#readme"
    exit 1
}

# Stop CAS
stop_cas() {
    if [ -n "$BAZEL_REMOTE_PID" ]; then
        log_info "Stopping local bazel-remote..."
        kill $BAZEL_REMOTE_PID 2>/dev/null || true
        wait $BAZEL_REMOTE_PID 2>/dev/null || true
    fi
    
    # Also stop Docker container if we started it
    if command -v docker &> /dev/null; then
        docker rm -f ferrisrbe-benchmark-cas 2>/dev/null || true
    fi
}

# Get server binary
get_server_binary() {
    # Try cargo build first
    if [ -f "$PROJECT_ROOT/target/release/rbe-server" ]; then
        echo "$PROJECT_ROOT/target/release/rbe-server"
        return 0
    fi
    
    # Try Bazel
    local bazel_bin=$(get_bazel_bin "$PROJECT_ROOT" 2>/dev/null)
    if [ -n "$bazel_bin" ] && [ -f "$bazel_bin/rbe-server" ]; then
        echo "$bazel_bin/rbe-server"
        return 0
    fi
    
    # Not found
    echo ""
    return 1
}

# Build server
build_server() {
    log_info "Building FerrisRBE server..."
    
    cd "$PROJECT_ROOT"
    
    if command -v cargo &> /dev/null; then
        log_info "Building with Cargo..."
        cargo build --release --bin rbe-server
    elif command -v bazel &> /dev/null; then
        log_info "Building with Bazel..."
        bazel build //:rbe-server --config=release
    else
        log_error "Neither Cargo nor Bazel found. Cannot build."
        exit 1
    fi
    
    log_success "Build complete"
}

# Start server
SERVER_PID=""
start_server() {
    local binary="$(get_server_binary)"
    
    if [ -z "$binary" ]; then
        log_info "Binary not found, building..."
        build_server
        binary="$(get_server_binary)"
    fi
    
    log_info "Starting server: $binary"
    
    export CAS_ENDPOINT="localhost:9094"
    "$binary" &
    SERVER_PID=$!
    
    # Wait for server
    log_info "Waiting for server to be ready..."
    for i in {1..30}; do
        if nc -z localhost 9092 2>/dev/null; then
            log_success "Server ready (PID: $SERVER_PID)"
            return 0
        fi
        sleep 1
    done
    
    log_error "Server failed to start"
    return 1
}

# Stop server
stop_server() {
    if [ -n "$SERVER_PID" ]; then
        log_info "Stopping server..."
        kill $SERVER_PID 2>/dev/null || true
        wait $SERVER_PID 2>/dev/null || true
    fi
}

# Cleanup on exit
cleanup() {
    log_info "Cleaning up..."
    stop_server
    stop_cas
}
trap cleanup EXIT

# Run memory benchmark
run_memory_benchmark() {
    log_info "Running memory benchmark..."
    
    for i in {1..5}; do
        ps -o rss= -p $SERVER_PID 2>/dev/null | awk '{print $1/1024}' || echo "0"
        sleep 1
    done | tee "$RESULTS_DIR/memory_${TIMESTAMP}.txt"
    
    local memory=$(grep -v "^0$" "$RESULTS_DIR/memory_${TIMESTAMP}.txt" | head -1)
    [ -z "$memory" ] && memory="0"
    
    echo "$memory" > "$RESULTS_DIR/memory_baseline.txt"
    log_success "Memory baseline: ${memory}MB"
}

# Run quick benchmarks
run_benchmarks() {
    log_info "=== LOCAL BENCHMARK SUITE ($MODE) ==="
    
    # 1. Memory
    log_info "Test 1/3: Memory footprint..."
    run_memory_benchmark
    
    # 2. Execution throughput
    log_info "Test 2/3: Execution throughput..."
    python3 "$SCRIPT_DIR/execution-load-test.py" \
        --server localhost:9092 \
        --actions 100 \
        --concurrent 10 \
        --output "$RESULTS_DIR/execution_${TIMESTAMP}.json" || {
        log_warn "Execution test failed (CAS may be unavailable)"
    }
    
    # 3. Action cache
    log_info "Test 3/3: Action cache performance..."
    python3 "$SCRIPT_DIR/action-cache-test.py" \
        --server localhost:9092 \
        --operations 1000 \
        --concurrent 20 \
        --operation read \
        --output "$RESULTS_DIR/cache_${TIMESTAMP}.json" || {
        log_warn "Cache test failed (CAS may be unavailable)"
    }
}

# Generate summary
generate_summary() {
    log_info "Generating summary..."
    
    local summary_file="$RESULTS_DIR/benchmark_summary.md"
    
    cat > "$summary_file" << EOF
### Local Benchmark Results

**Mode:** ${MODE}  
**Timestamp:** $(date -u +"%Y-%m-%d %H:%M:%S UTC")

EOF
    
    if [ -f "$RESULTS_DIR/memory_baseline.txt" ]; then
        local memory=$(cat "$RESULTS_DIR/memory_baseline.txt")
        echo "#### Memory Footprint" >> "$summary_file"
        echo "- **Idle Memory:** ${memory} MB" >> "$summary_file"
        echo "" >> "$summary_file"
        
        if command -v bc &> /dev/null; then
            if (( $(echo "$memory > 20" | bc -l) )); then
                echo "⚠️ **WARNING:** Memory usage (${memory}MB) exceeds threshold (20MB)" >> "$summary_file"
            else
                echo "✅ Memory usage within expected range" >> "$summary_file"
            fi
        fi
        echo "" >> "$summary_file"
    fi
    
    echo "#### Test Results" >> "$summary_file"
    for json in "$RESULTS_DIR"/*.json; do
        if [ -f "$json" ]; then
            local basename=$(basename "$json" .json)
            echo "- ${basename}: ✅ Completed" >> "$summary_file"
        fi
    done
    
    echo "" >> "$summary_file"
    echo "---" >> "$summary_file"
    echo "*Generated by FerrisRBE Local Benchmark*" >> "$summary_file"
    
    cat "$summary_file"
}

# Main
main() {
    log_info "========================================"
    log_info "FerrisRBE Local Benchmark"
    log_info "========================================"
    
    check_prerequisites
    start_cas
    start_server
    run_benchmarks
    generate_summary
    
    log_success "Benchmark complete!"
    log_info "Results: $RESULTS_DIR/"
}

main "$@"
