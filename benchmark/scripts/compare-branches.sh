#!/bin/bash
# Compare benchmark results between PR (local build) and Official Release (latest tag)
# 
# This script compares the current PR's OCI image against the official Docker Hub
# image (xangcastle/ferris-server:latest), providing a realistic comparison of
# what users will experience when upgrading.
#
# Benefits over compiling main:
# - Faster (pull vs build): ~30 seconds vs ~5-10 minutes
# - Tests the actual release artifact (same as production)
# - Detects issues in the OCI build process
# - More realistic user experience comparison
#
# Usage: ./compare-branches.sh [pr_image_tag]
#   pr_image_tag: Tag for PR image (default: ferrisrbe/server:latest)

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCHMARK_DIR="$(dirname "$SCRIPT_DIR")"
RESULTS_DIR="$BENCHMARK_DIR/results"

# Image tags
PR_IMAGE_TAG="${1:-ferrisrbe/server:latest}"
OFFICIAL_IMAGE_TAG="xangcastle/ferris-server:latest"

# Container names
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
OFFICIAL_CONTAINER="ferrisrbe-official-${TIMESTAMP}"
PR_CONTAINER="ferrisrbe-pr-${TIMESTAMP}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m'

log_info() { echo -e "${BLUE}[INFO]${NC} $1"; }
log_success() { echo -e "${GREEN}[SUCCESS]${NC} $1"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }
log_error() { echo -e "${RED}[ERROR]${NC} $1"; }
log_section() { echo -e "${CYAN}$1${NC}"; }

mkdir -p "$RESULTS_DIR"

cleanup() {
    log_info "Cleaning up comparison containers..."
    
    # Stop and remove containers
    docker stop "$OFFICIAL_CONTAINER" "$PR_CONTAINER" >/dev/null 2>&1 || true
    docker rm "$OFFICIAL_CONTAINER" "$PR_CONTAINER" >/dev/null 2>&1 || true
    
    log_success "Cleanup complete"
}

trap cleanup EXIT

# Wait for bazel-remote if using GitHub Actions services
wait_for_services() {
    if [ -n "$BENCHMARK_SERVICES" ]; then
        log_info "Waiting for bazel-remote service..."
        for i in {1..60}; do
            if nc -z localhost 9094 2>/dev/null; then
                log_success "bazel-remote is ready on port 9094"
                break
            fi
            if [ $((i % 10)) -eq 0 ]; then
                log_info "  Waiting... (attempt $i/60)"
            fi
            sleep 2
        done
    fi
    
    # Ensure CAS_ENDPOINT is set
    if [ -z "$CAS_ENDPOINT" ]; then
        export CAS_ENDPOINT="localhost:9094"
    fi
    
    log_info "Using CAS_ENDPOINT: $CAS_ENDPOINT"
}

# Pull official image from Docker Hub
pull_official_image() {
    log_section "=== Pulling Official Image ==="
    log_info "Pulling: $OFFICIAL_IMAGE_TAG"
    
    if ! docker pull "$OFFICIAL_IMAGE_TAG" 2>&1; then
        log_error "Failed to pull official image from Docker Hub"
        log_info "This could mean:"
        log_info "  - No internet connectivity"
        log_info "  - Docker Hub rate limiting"
        log_info "  - Image doesn't exist (first release?)"
        exit 1
    fi
    
    # Get image info
    local image_id=$(docker images --format "{{.ID}}" "$OFFICIAL_IMAGE_TAG" | head -1)
    local image_created=$(docker images --format "{{.CreatedAt}}" "$OFFICIAL_IMAGE_TAG" | head -1)
    local image_size=$(docker images --format "{{.Size}}" "$OFFICIAL_IMAGE_TAG" | head -1)
    
    log_success "Official image pulled successfully"
    log_info "  Image ID: $image_id"
    log_info "  Created: $image_created"
    log_info "  Size: $image_size"
    
    # Save info for report
    echo "$image_id" > "$RESULTS_DIR/official_image_id.txt"
    echo "$image_created" > "$RESULTS_DIR/official_image_created.txt"
    echo "$image_size" > "$RESULTS_DIR/official_image_size.txt"
}

# Verify PR image exists
verify_pr_image() {
    log_section "=== Verifying PR Image ==="
    log_info "Checking: $PR_IMAGE_TAG"
    
    if ! docker image inspect "$PR_IMAGE_TAG" >/dev/null 2>&1; then
        log_error "PR image $PR_IMAGE_TAG not found in Docker"
        log_info "Available ferrisrbe images:"
        docker images | grep ferrisrbe || true
        log_info ""
        log_info "Build the PR image with:"
        log_info "  bazel run //oci:server_load_amd64"
        exit 1
    fi
    
    # Get image info
    local image_id=$(docker images --format "{{.ID}}" "$PR_IMAGE_TAG" | head -1)
    local image_created=$(docker images --format "{{.CreatedAt}}" "$PR_IMAGE_TAG" | head -1)
    local image_size=$(docker images --format "{{.Size}}" "$PR_IMAGE_TAG" | head -1)
    
    log_success "PR image verified"
    log_info "  Image ID: $image_id"
    log_info "  Created: $image_created"
    log_info "  Size: $image_size"
    
    # Save info for report
    echo "$image_id" > "$RESULTS_DIR/pr_image_id.txt"
    echo "$image_created" > "$RESULTS_DIR/pr_image_created.txt"
    echo "$image_size" > "$RESULTS_DIR/pr_image_size.txt"
}

# Function to benchmark a container image
benchmark_image() {
    local image_tag="$1"
    local name="$2"
    local container_name="$3"
    local output_dir="$RESULTS_DIR/${name}"
    
    mkdir -p "$output_dir"
    
    log_section "=== Benchmarking: $name ==="
    log_info "Image: $image_tag"
    log_info "Container: $container_name"
    
    # Start container
    log_info "Starting container..."
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
        "$image_tag" >/dev/null 2>&1
    
    # Wait for server to be ready
    log_info "Waiting for server to be ready..."
    local ready=false
    for i in {1..60}; do
        if nc -z localhost 9092 2>/dev/null; then
            ready=true
            break
        fi
        if [ $((i % 10)) -eq 0 ]; then
            log_info "  Waiting... (attempt $i/60)"
            docker logs "$container_name" --tail 3 2>/dev/null || true
        fi
        sleep 1
    done
    
    if [ "$ready" != "true" ]; then
        log_error "Server failed to start within 60 seconds"
        docker logs "$container_name" 2>/dev/null || true
        docker stop "$container_name" >/dev/null 2>&1 || true
        docker rm "$container_name" >/dev/null 2>&1 || true
        return 1
    fi
    
    log_success "Server ready"
    
    # Wait for stabilization
    sleep 2
    
    # Get memory baseline (multiple samples)
    log_info "Sampling memory usage..."
    for i in {1..5}; do
        docker stats "$container_name" --no-stream --format "{{.MemUsage}}" 2>/dev/null | \
            awk '{print $1}' | sed 's/MiB//' | sed 's/GiB/*1024/' | bc 2>/dev/null || echo "0"
        sleep 1
    done > "$output_dir/memory_samples.txt"
    
    # Use first non-zero value
    MEMORY=$(grep -v "^0$" "$output_dir/memory_samples.txt" | head -1)
    [ -z "$MEMORY" ] && MEMORY="0"
    
    echo "$MEMORY" > "$output_dir/memory.txt"
    log_info "Memory: ${MEMORY}MB"
    
    # Quick throughput test
    log_info "Running throughput test..."
    python3 "$SCRIPT_DIR/execution-load-test.py" \
        --server localhost:9092 \
        --actions 50 \
        --concurrent 10 \
        --output "$output_dir/execution.json" 2>/dev/null || true
    
    # Get container stats
    docker stats "$container_name" --no-stream --format "table {{.CPUPerc}}\t{{.MemUsage}}\t{{.NetIO}}" > "$output_dir/container_stats.txt" 2>/dev/null || true
    
    # Stop container
    log_info "Stopping container..."
    docker stop "$container_name" >/dev/null 2>&1 || true
    docker rm "$container_name" >/dev/null 2>&1 || true
    
    sleep 2
    
    log_success "Benchmark for $name complete"
}

# Generate comparison report
generate_comparison_report() {
    log_section "=== Generating Comparison Report ==="
    
    COMPARISON_FILE="$RESULTS_DIR/comparison.md"
    
    # Read results
    local official_mem=$(cat "$RESULTS_DIR/official/memory.txt" 2>/dev/null || echo "N/A")
    local pr_mem=$(cat "$RESULTS_DIR/pr/memory.txt" 2>/dev/null || echo "N/A")
    local official_size=$(cat "$RESULTS_DIR/official_image_size.txt" 2>/dev/null || echo "N/A")
    local pr_size=$(cat "$RESULTS_DIR/pr_image_size.txt" 2>/dev/null || echo "N/A")
    local official_id=$(cat "$RESULTS_DIR/official_image_id.txt" 2>/dev/null || echo "N/A")
    local pr_id=$(cat "$RESULTS_DIR/pr_image_id.txt" 2>/dev/null || echo "N/A")
    
    cat > "$COMPARISON_FILE" << EOF
## 📊 Performance Comparison: PR vs Official Release

### Container Images

| Attribute | Official (latest) | PR | Notes |
|-----------|-------------------|-----|-------|
| **Image** | \`$OFFICIAL_IMAGE_TAG\` | \`$PR_IMAGE_TAG\` | - |
| **Image ID** | \`${official_id:0:12}\` | \`${pr_id:0:12}\` | Different = new build |
| **Size** | $official_size | $pr_size | Smaller is better |

### Performance Metrics

| Metric | Official (latest) | PR | Change | Status |
|--------|-------------------|-----|--------|--------|
EOF
    
    # Compare memory
    if [ "$official_mem" != "N/A" ] && [ "$pr_mem" != "N/A" ] && \
       [ "$official_mem" != "0" ] && [ "$pr_mem" != "0" ]; then
        
        # Calculate change
        CHANGE=$(echo "scale=2; (($pr_mem - $official_mem) / $official_mem) * 100" | bc)
        
        # Determine status
        if (( $(echo "$CHANGE <= 5" | bc -l) )); then
            STATUS="✅"
        elif (( $(echo "$CHANGE <= 15" | bc -l) )); then
            STATUS="⚠️"
        else
            STATUS="🚨"
        fi
        
        ARROW=$(echo "$CHANGE" | awk '{if ($1 < 0) print "↓"; else if ($1 > 0) print "↑"; else print "="}')
        
        echo "| Memory (MB) | ${official_mem} | ${pr_mem} | ${ARROW} ${CHANGE}% | ${STATUS} |" >> "$COMPARISON_FILE"
    else
        echo "| Memory (MB) | ${official_mem} | ${pr_mem} | N/A | ⚠️ |" >> "$COMPARISON_FILE"
    fi
    
    cat >> "$COMPARISON_FILE" << EOF

#### Legend
- ✅ Within 5% - Acceptable
- ⚠️ 5-15% change - Review recommended  
- 🚨 >15% regression - Optimization required

#### Interpretation
EOF
    
    # Add interpretation
    if [ "$pr_mem" != "N/A" ] && [ "$pr_mem" != "0" ]; then
        if (( $(echo "$pr_mem > 20" | bc -l) )); then
            echo "- 🚨 **Memory regression detected**: PR uses ${pr_mem}MB vs expected <20MB" >> "$COMPARISON_FILE"
        else
            echo "- ✅ **Memory usage acceptable**: ${pr_mem}MB within expected range" >> "$COMPARISON_FILE"
        fi
    fi
    
    # Image size comparison
    if [ "$official_size" != "N/A" ] && [ "$pr_size" != "N/A" ]; then
        echo "" >> "$COMPARISON_FILE"
        echo "#### Image Size Analysis" >> "$COMPARISON_FILE"
        
        # Extract numeric values for comparison (rough approximation)
        local official_size_mb=$(echo "$official_size" | sed 's/MB//' | sed 's/GB/*1024/' | bc 2>/dev/null || echo "0")
        local pr_size_mb=$(echo "$pr_size" | sed 's/MB//' | sed 's/GB/*1024/' | bc 2>/dev/null || echo "0")
        
        if [ "$pr_size_mb" != "0" ] && [ "$official_size_mb" != "0" ]; then
            if (( $(echo "$pr_size_mb > $official_size_mb" | bc -l) )); then
                echo "- ⚠️ PR image is larger than official (${pr_size} vs ${official_size})" >> "$COMPARISON_FILE"
            elif (( $(echo "$pr_size_mb < $official_size_mb" | bc -l) )); then
                echo "- ✅ PR image is smaller than official (${pr_size} vs ${official_size}) - Good optimization!" >> "$COMPARISON_FILE"
            else
                echo "- ✅ Image size unchanged (${pr_size})" >> "$COMPARISON_FILE"
            fi
        fi
    fi
    
    cat >> "$COMPARISON_FILE" << EOF

---
*Comparison generated at: $(date -u +"%Y-%m-%d %H:%M:%S UTC")*
EOF
    
    log_success "Comparison report generated: $COMPARISON_FILE"
    
    # Display the comparison
    echo ""
    cat "$COMPARISON_FILE"
}

# Print summary
print_summary() {
    log_section "========================================"
    log_section "     Comparison Complete!"
    log_section "========================================"
    log_info "Official Image: $OFFICIAL_IMAGE_TAG"
    log_info "PR Image:       $PR_IMAGE_TAG"
    log_info "Results:        $RESULTS_DIR"
    echo ""
    log_info "Key files:"
    log_info "  - $RESULTS_DIR/comparison.md (main report)"
    log_info "  - $RESULTS_DIR/official/ (official image results)"
    log_info "  - $RESULTS_DIR/pr/ (PR image results)"
}

# Main execution
main() {
    log_section "========================================"
    log_section "Container Image Comparison Benchmark"
    log_section "========================================"
    log_info "Comparing PR against official Docker Hub release"
    echo ""
    
    # Check Docker is available
    if ! command -v docker &> /dev/null; then
        log_error "Docker is not installed or not in PATH"
        exit 1
    fi
    
    # Wait for services
    wait_for_services
    
    # Pull official image
    pull_official_image
    
    # Verify PR image
    verify_pr_image
    
    echo ""
    log_section "========================================"
    
    # Benchmark official image first
    benchmark_image "$OFFICIAL_IMAGE_TAG" "official" "$OFFICIAL_CONTAINER"
    
    echo ""
    log_section "========================================"
    
    # Benchmark PR image
    benchmark_image "$PR_IMAGE_TAG" "pr" "$PR_CONTAINER"
    
    echo ""
    log_section "========================================"
    
    # Generate comparison
    generate_comparison_report
    
    # Print summary
    print_summary
}

main "$@"
