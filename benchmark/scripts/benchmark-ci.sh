#!/bin/bash
# CI/CD Benchmark Script - Container-Native Version
# Runs benchmarks using Docker containers (like production deployment)
# Works with any Bazel configuration (--symlink_prefix)
#
# This script tests the actual OCI images that get deployed to Kubernetes,
# providing more realistic benchmarks than native binary execution.

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCHMARK_DIR="$(dirname "$SCRIPT_DIR")"

# Source Bazel utilities
source "$SCRIPT_DIR/bazel-utils.sh"

PROJECT_ROOT="$(get_workspace_root "$BENCHMARK_DIR/..")"
RESULTS_DIR="$BENCHMARK_DIR/results"
MODE="${1:-light}"

# Container configuration
CONTAINER_NAME="ferrisrbe-benchmark"
NETWORK_NAME="ferrisrbe-benchmark-net"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
CONTAINER_NAME_FULL="${CONTAINER_NAME}-${TIMESTAMP}"

# Image tags
LOCAL_IMAGE_TAG="ferrisrbe/server:latest"
OFFICIAL_IMAGE_TAG="xangcastle/ferris-server:latest"

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

cleanup() {
    log_info "Cleaning up containers and network..."
    
    # Stop and remove benchmark container
    if docker ps -q --filter "name=${CONTAINER_NAME_FULL}" | grep -q .; then
        log_info "Stopping benchmark container..."
        docker stop "${CONTAINER_NAME_FULL}" >/dev/null 2>&1 || true
        docker rm "${CONTAINER_NAME_FULL}" >/dev/null 2>&1 || true
    fi
    
    # Stop and remove any lingering containers with our prefix
    docker ps -aq --filter "name=${CONTAINER_NAME}-" | xargs -r docker rm -f >/dev/null 2>&1 || true
    
    # Remove network if it exists
    if docker network ls --format '{{.Name}}' | grep -q "^${NETWORK_NAME}$"; then
        docker network rm "${NETWORK_NAME}" >/dev/null 2>&1 || true
    fi
    
    # Stop supporting services if we started them
    stop_services
    
    log_success "Cleanup complete"
}

trap cleanup EXIT

# Build FerrisRBE OCI image using Bazel (dogfooding)
build_image() {
    log_info "Building FerrisRBE OCI image with Bazel..."
    
    cd "$PROJECT_ROOT"
    
    # Detect architecture
    local arch=$(uname -m)
    local load_target="//oci:server_load_amd64"
    
    if [ "$arch" = "arm64" ] || [ "$arch" = "aarch64" ]; then
        load_target="//oci:server_load"
        log_info "Detected ARM64 architecture"
    else
        log_info "Detected AMD64 architecture"
    fi
    
    # Build and load image to Docker
    log_info "Building and loading OCI image: $load_target"
    if ! bazel run "$load_target" 2>&1; then
        log_error "Failed to build and load OCI image"
        log_info "Make sure Bazel is installed and configured"
        exit 1
    fi
    
    # Verify image was loaded
    if ! docker image inspect "$LOCAL_IMAGE_TAG" >/dev/null 2>&1; then
        log_error "Image $LOCAL_IMAGE_TAG not found in Docker"
        exit 1
    fi
    
    log_success "OCI image built and loaded: $LOCAL_IMAGE_TAG"
    
    # Get image size
    local image_size=$(docker images --format "{{.Size}}" "$LOCAL_IMAGE_TAG" | head -1)
    log_info "Image size: $image_size"
}

# Start supporting services (bazel-remote for CAS)
start_services() {
    log_info "Starting supporting services..."
    
    # Check if we're using GitHub Actions services
    if [ -n "$BENCHMARK_SERVICES" ]; then
        log_info "Using GitHub Actions services for bazel-remote..."
        # Wait for the service to be ready (GitHub Actions starts it automatically)
        log_info "Waiting for bazel-remote to initialize..."
        for i in {1..90}; do
            if nc -z localhost 9094 2>/dev/null; then
                log_success "CAS (bazel-remote) is ready on port 9094"
                sleep 3
                return 0
            fi
            if [ $((i % 10)) -eq 0 ]; then
                log_info "Still waiting for bazel-remote... (attempt $i/90)"
            fi
            sleep 2
        done
        log_warn "bazel-remote did not become ready in time"
        return 1
    fi
    
    # Check if we're in local mode (auto-detect and start bazel-remote if needed)
    if [ -n "$BENCHMARK_LOCAL" ] || [ -z "$BENCHMARK_SERVICES" ]; then
        log_info "Checking for bazel-remote..."
        
        # Check if bazel-remote is already running (container or native)
        if docker ps --filter "name=bazel-remote" --format '{{.Names}}' | grep -q "bazel-remote" || \
           nc -z localhost 9094 2>/dev/null; then
            log_success "CAS (bazel-remote) already running on port 9094"
            return 0
        fi
        
        # Try to start bazel-remote in Docker
        start_bazel_remote_container
        return $?
    fi
    
    # Verify CAS is available
    if nc -z localhost 9094 2>/dev/null; then
        log_success "CAS (bazel-remote) available on port 9094"
    else
        log_warn "CAS not available on port 9094, some tests may fail"
    fi
}

# Start bazel-remote in a container
start_bazel_remote_container() {
    log_info "Starting bazel-remote container..."
    
    # Check if a container already exists (stopped)
    if docker ps -aq --filter "name=bazel-remote" | grep -q .; then
        log_info "Removing existing bazel-remote container..."
        docker rm -f bazel-remote >/dev/null 2>&1 || true
    fi
    
    # Create data directory
    mkdir -p "$RESULTS_DIR/bazel-remote-cache"
    
    # Start container
    docker run -d \
        --name bazel-remote \
        --network host \
        -p 9094:9094 \
        -p 8080:8080 \
        -v "$RESULTS_DIR/bazel-remote-cache:/data" \
        -e BAZEL_REMOTE_DIR=/data \
        -e BAZEL_REMOTE_MAX_SIZE=1 \
        buchgr/bazel-remote-cache:latest \
        --port=9094 \
        --grpc_port=9094 \
        --dir=/data \
        --max_size=1 >/dev/null 2>&1
    
    BAZEL_REMOTE_CONTAINER="bazel-remote"
    
    # Wait for bazel-remote to be ready
    for i in {1..60}; do
        if nc -z localhost 9094 2>/dev/null; then
            log_success "bazel-remote container ready"
            return 0
        fi
        if [ $((i % 10)) -eq 0 ]; then
            log_info "Waiting for bazel-remote... (attempt $i/60)"
        fi
        sleep 1
    done
    
    log_warn "bazel-remote failed to start within 60 seconds"
    docker logs bazel-remote 2>/dev/null || true
    return 1
}

# Stop supporting services
stop_services() {
    if [ -n "$BAZEL_REMOTE_CONTAINER" ]; then
        log_info "Stopping bazel-remote container..."
        docker stop "$BAZEL_REMOTE_CONTAINER" >/dev/null 2>&1 || true
        docker rm "$BAZEL_REMOTE_CONTAINER" >/dev/null 2>&1 || true
    fi
}

# Start FerrisRBE server in container
start_server_container() {
    local image_tag="${1:-$LOCAL_IMAGE_TAG}"
    local container_name="${CONTAINER_NAME_FULL}"
    
    log_info "Starting FerrisRBE server container: $image_tag"
    
    # Verify image exists
    if ! docker image inspect "$image_tag" >/dev/null 2>&1; then
        log_error "Image $image_tag not found in Docker"
        log_info "Available images:"
        docker images | grep ferrisrbe || true
        exit 1
    fi
    
    # Ensure CAS_ENDPOINT is set
    if [ -z "$CAS_ENDPOINT" ]; then
        export CAS_ENDPOINT="localhost:9094"
    fi
    
    log_info "Using CAS_ENDPOINT: $CAS_ENDPOINT"
    
    # Run container with host networking for simplicity
    # This matches how many CI environments work
    docker run -d \
        --name "$container_name" \
        --network host \
        -p 9092:9092 \
        -e RBE_PORT=9092 \
        -e RBE_BIND_ADDRESS=0.0.0.0 \
        -e RUST_LOG=info \
        -e CAS_ENDPOINT="$CAS_ENDPOINT" \
        -e RBE_L1_CACHE_CAPACITY=100000 \
        -e RBE_L1_CACHE_TTL_SECS=3600 \
        --memory=512m \
        --memory-reservation=64m \
        "$image_tag" >/dev/null 2>&1
    
    SERVER_CONTAINER="$container_name"
    
    # Wait for server to be ready
    log_info "Waiting for server to be ready..."
    for i in {1..60}; do
        if nc -z localhost 9092 2>/dev/null; then
            log_success "Server container ready"
            return 0
        fi
        if [ $((i % 10)) -eq 0 ]; then
            log_info "Waiting for server... (attempt $i/60)"
            docker logs "$container_name" --tail 5 2>/dev/null || true
        fi
        sleep 1
    done
    
    log_error "Server failed to start within 60 seconds"
    docker logs "$container_name" 2>/dev/null || true
    exit 1
}

# Stop server container
stop_server_container() {
    local container_name="${1:-$SERVER_CONTAINER}"
    
    if [ -n "$container_name" ]; then
        log_info "Stopping server container: $container_name"
        docker stop "$container_name" >/dev/null 2>&1 || true
        docker rm "$container_name" >/dev/null 2>&1 || true
    fi
}

# Get container memory usage
get_container_memory() {
    local container_name="${1:-$SERVER_CONTAINER}"
    
    # Get memory usage in MB from docker stats
    docker stats "$container_name" --no-stream --format "{{.MemUsage}}" 2>/dev/null | \
        awk '{print $1}' | sed 's/MiB//' | sed 's/GiB/*1024/' | bc 2>/dev/null || echo "0"
}

# Run memory benchmark using container stats
run_memory_benchmark() {
    log_info "Running memory footprint benchmark (container mode)..."
    
    # Wait for container to stabilize
    sleep 3
    
    # Collect multiple samples
    for i in {1..5}; do
        get_container_memory "$SERVER_CONTAINER"
        sleep 1
    done | tee "$RESULTS_DIR/memory_${TIMESTAMP}.txt"
    
    # Parse and store result (median of non-zero values)
    MEMORY_MB=$(grep -v "^0$" "$RESULTS_DIR/memory_${TIMESTAMP}.txt" | sort -n | head -1)
    if [ -z "$MEMORY_MB" ]; then
        MEMORY_MB="0"
    fi
    
    echo "$MEMORY_MB" > "$RESULTS_DIR/memory_baseline.txt"
    
    log_success "Memory baseline: ${MEMORY_MB}MB (container)"
    
    # Also get container stats for additional info
    docker stats "$SERVER_CONTAINER" --no-stream --format "table {{.Container}}\t{{.CPUPerc}}\t{{.MemUsage}}\t{{.NetIO}}\t{{.BlockIO}}" > "$RESULTS_DIR/container_stats_${TIMESTAMP}.txt" 2>/dev/null || true
}

# Run lightweight benchmarks
run_light_benchmarks() {
    log_info "=== LIGHTWEIGHT BENCHMARK SUITE (Container) ==="
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
    
    # 4. Cold start (single measurement) - measured AFTER bazel-remote is ready
    log_info "Test 4/5: Cold start time (container)..."
    
    # Ensure bazel-remote is ready before measuring cold start
    log_info "  Verifying bazel-remote is ready..."
    if ! nc -z localhost 9094 2>/dev/null; then
        log_warn "  bazel-remote not ready, waiting..."
        for i in {1..30}; do
            if nc -z localhost 9094 2>/dev/null; then
                break
            fi
            sleep 1
        done
    fi
    
    if nc -z localhost 9094 2>/dev/null; then
        log_info "  bazel-remote ready, measuring container cold start..."
        
        # Stop current container
        stop_server_container "$SERVER_CONTAINER"
        sleep 1
        
        # Measure container startup time
        START_TIME=$(date +%s%N)
        
        # Start new container
        NEW_CONTAINER="${CONTAINER_NAME}-coldstart-${TIMESTAMP}"
        docker run -d \
            --name "$NEW_CONTAINER" \
            --network host \
            -p 9092:9092 \
            -e RBE_PORT=9092 \
            -e RBE_BIND_ADDRESS=0.0.0.0 \
            -e RUST_LOG=info \
            -e CAS_ENDPOINT="$CAS_ENDPOINT" \
            "$LOCAL_IMAGE_TAG" >/dev/null 2>&1
        
        # Wait for server to be ready
        for i in {1..60}; do
            if nc -z localhost 9092 2>/dev/null; then
                END_TIME=$(date +%s%N)
                COLD_START_MS=$(( (END_TIME - START_TIME) / 1000000 ))
                echo "$COLD_START_MS" > "$RESULTS_DIR/coldstart_${TIMESTAMP}.txt"
                log_success "Cold start: ${COLD_START_MS}ms (container startup + server ready)"
                break
            fi
            sleep 0.1
        done
        
        # Update SERVER_CONTAINER to new one
        stop_server_container "$SERVER_CONTAINER"
        SERVER_CONTAINER="$NEW_CONTAINER"
        
        if ! nc -z localhost 9092 2>/dev/null; then
            log_error "Server container failed to start for cold start measurement"
            stop_server_container "$NEW_CONTAINER"
        fi
    else
        log_warn "  bazel-remote not available, skipping cold start measurement"
        echo "0" > "$RESULTS_DIR/coldstart_${TIMESTAMP}.txt"
    fi
    
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
    log_info "=== FULL BENCHMARK SUITE (Container) ==="
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
        --container "$SERVER_CONTAINER" \
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
    
    # Get container image info
    local image_id=$(docker images --format "{{.ID}}" "$LOCAL_IMAGE_TAG" | head -1)
    local image_created=$(docker images --format "{{.CreatedAt}}" "$LOCAL_IMAGE_TAG" | head -1)
    local image_size=$(docker images --format "{{.Size}}" "$LOCAL_IMAGE_TAG" | head -1)
    
    cat > "$SUMMARY_FILE" << EOF
### Benchmark Results Summary (Container Mode)

**Mode:** ${MODE}  
**Timestamp:** $(date -u +"%Y-%m-%d %H:%M:%S UTC")  
**Commit:** ${GITHUB_SHA:-N/A}  
**Image:** ${LOCAL_IMAGE_TAG}  
**Image ID:** ${image_id:-N/A}  
**Image Size:** ${image_size:-N/A}

EOF
    
    # Memory results
    if [ -f "$RESULTS_DIR/memory_baseline.txt" ]; then
        MEMORY=$(cat "$RESULTS_DIR/memory_baseline.txt")
        echo "#### Memory Footprint (Container)" >> "$SUMMARY_FILE"
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
        echo "#### Cold Start Time (Container)" >> "$SUMMARY_FILE"
        echo "- **Startup Time:** ${COLD_START}ms" >> "$SUMMARY_FILE"
        echo "" >> "$SUMMARY_FILE"
        
        if [ "$COLD_START" -gt 500 ]; then
            echo "⚠️ **WARNING:** Cold start (${COLD_START}ms) exceeds threshold (500ms)" >> "$SUMMARY_FILE"
        else
            echo "✅ Cold start within expected range" >> "$SUMMARY_FILE"
        fi
        echo "" >> "$SUMMARY_FILE"
    fi
    
    # Container stats
    if [ -f "$RESULTS_DIR/container_stats_${TIMESTAMP}.txt" ]; then
        echo "#### Container Statistics" >> "$SUMMARY_FILE"
        echo "\`\`\`" >> "$SUMMARY_FILE"
        cat "$RESULTS_DIR/container_stats_${TIMESTAMP}.txt" >> "$SUMMARY_FILE"
        echo "\`\`\`" >> "$SUMMARY_FILE"
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
    echo "*Generated by FerrisRBE Benchmark Suite (Container Mode)*" >> "$SUMMARY_FILE"
    
    # Also generate JSON for programmatic access
    cat > "$RESULTS_DIR/benchmark_data.json" << EOF
{
    "timestamp": "$TIMESTAMP",
    "mode": "$MODE",
    "commit": "${GITHUB_SHA:-N/A}",
    "container": {
        "image": "$LOCAL_IMAGE_TAG",
        "image_id": "${image_id:-null}",
        "image_size": "${image_size:-null}",
        "container_name": "$SERVER_CONTAINER"
    },
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
    log_info "Starting CI Benchmark Suite (Container Mode)"
    log_info "Mode: $MODE"
    log_info "Results directory: $RESULTS_DIR"
    
    # Check Docker is available
    if ! command -v docker &> /dev/null; then
        log_error "Docker is not installed or not in PATH"
        exit 1
    fi
    
    log_info "Docker version: $(docker --version)"
    
    # Build OCI image
    build_image
    
    # Start supporting services first
    start_services
    
    # Start server container
    start_server_container "$LOCAL_IMAGE_TAG"
    
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
