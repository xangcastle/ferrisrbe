#!/bin/bash
# RBE Memory Footprint Benchmark
# Professional benchmark script with proper metric collection

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RESULTS_DIR="${SCRIPT_DIR}/../results"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)

mkdir -p "$RESULTS_DIR"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() { echo -e "${BLUE}[INFO]${NC} $1"; }
log_success() { echo -e "${GREEN}[SUCCESS]${NC} $1"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }

# Parse memory value from docker stats to MB
parse_memory_mb() {
    local mem_str="$1"
    # Extract first value (before /)
    local value=$(echo "$mem_str" | awk '{print $1}')
    
    # Convert to MB
    if [[ "$value" == *"GiB" ]]; then
        echo "$value" | sed 's/GiB//' | awk '{printf "%.1f", $1 * 1024}'
    elif [[ "$value" == *"MiB" ]]; then
        echo "$value" | sed 's/MiB//'
    elif [[ "$value" == *"KiB" ]]; then
        echo "$value" | sed 's/KiB//' | awk '{printf "%.1f", $1 / 1024}'
    else
        echo "0"
    fi
}

# Run benchmark for a single solution
run_benchmark() {
    local name="$1"
    local image="$2"
    local port="$3"
    local env_vars="${4:-}"
    
    log_info "Benchmarking $name..."
    
    # Run container
    local container_name="bench-$(echo "$name" | tr '[:upper:]' '[:lower:]')-${TIMESTAMP}"
    local run_cmd="docker run -d --name $container_name --network benchmark-network -p $port:$port"
    
    if [ -n "$env_vars" ]; then
        run_cmd="$run_cmd $env_vars"
    fi
    
    run_cmd="$run_cmd $image"
    
    eval "$run_cmd" 2>/dev/null || {
        log_warn "Failed to start $name container"
        echo "$name,N/A,N/A,N/A,failed" >> "$RESULTS_DIR/benchmark_${TIMESTAMP}.csv"
        return 1
    }
    
    # Wait for initialization
    sleep 5
    
    # Collect multiple samples
    local samples=()
    local cpu_samples=()
    
    for i in {1..5}; do
        local stats=$(docker stats "$container_name" --no-stream --format "{{.MemUsage}},{{.MemPerc}},{{.CPUPerc}}" 2>/dev/null)
        if [ -n "$stats" ]; then
            local mem_str=$(echo "$stats" | cut -d',' -f1)
            local mem_mb=$(parse_memory_mb "$mem_str")
            local cpu_pct=$(echo "$stats" | cut -d',' -f3 | sed 's/%//')
            
            samples+=("$mem_mb")
            cpu_samples+=("$cpu_pct")
        fi
        sleep 1
    done
    
    # Calculate average
    local avg_mem=0
    local avg_cpu=0
    
    if [ ${#samples[@]} -gt 0 ]; then
        local sum=0
        for s in "${samples[@]}"; do
            sum=$(echo "$sum + $s" | bc)
        done
        avg_mem=$(echo "scale=1; $sum / ${#samples[@]}" | bc)
        
        local cpu_sum=0
        for c in "${cpu_samples[@]}"; do
            cpu_sum=$(echo "$cpu_sum + $c" | bc)
        done
        avg_cpu=$(echo "scale=2; $cpu_sum / ${#cpu_samples[@]}" | bc)
    fi
    
    # Get final stats
    local final_stats=$(docker stats "$container_name" --no-stream --format "{{.MemUsage}},{{.MemPerc}}" 2>/dev/null)
    local mem_limit=$(echo "$final_stats" | cut -d',' -f1 | awk -F'/' '{print $2}' | xargs)
    
    # Cleanup
    docker rm -f "$container_name" >/dev/null 2>&1
    
    # Output results
    echo "$name,$avg_mem,$mem_limit,$avg_cpu,success" >> "$RESULTS_DIR/benchmark_${TIMESTAMP}.csv"
    
    log_success "$name: ${avg_mem} MB (CPU: ${avg_cpu}%)"
}

# Main benchmark
main() {
    echo "========================================"
    echo "RBE Memory Footprint Benchmark"
    echo "Timestamp: $TIMESTAMP"
    echo "========================================"
    echo ""
    
    # Create network
    docker network create benchmark-network 2>/dev/null || true
    
    # Initialize CSV
    echo "solution,memory_mb,memory_limit,cpu_percent,status" > "$RESULTS_DIR/benchmark_${TIMESTAMP}.csv"
    
    # Check if FerrisRBE image exists
    if ! docker image inspect ferrisrbe-server:latest >/dev/null 2>&1; then
        log_warn "FerrisRBE image not found. Build with Bazel:"
        echo "  cd benchmark && ./scripts/build-with-bazel.sh image"
        echo ""
        echo "Or manually:"
        echo "  bazel build //oci:server_image"
        echo "  bazel run //oci:server_load"
        exit 1
    fi
    
    # Benchmark FerrisRBE
    run_benchmark "FerrisRBE" "ferrisrbe-server:latest" "9092" "-e RBE_PORT=9092 -e RBE_BIND_ADDRESS=0.0.0.0"
    
    # Cleanup network
    docker network rm benchmark-network 2>/dev/null || true
    
    # Generate report
    echo ""
    echo "========================================"
    echo "Results Summary"
    echo "========================================"
    column -s',' -t "$RESULTS_DIR/benchmark_${TIMESTAMP}.csv"
    echo ""
    echo "Full results saved to: $RESULTS_DIR/benchmark_${TIMESTAMP}.csv"
    
    # Generate markdown report
    cat > "$RESULTS_DIR/BENCHMARK_REPORT_${TIMESTAMP}.md" << EOF
# RBE Benchmark Report

**Date:** $(date -u +"%Y-%m-%d %H:%M:%S UTC")  
**Runner:** $(whoami)@$(hostname)

## Results

$(column -s',' -t "$RESULTS_DIR/benchmark_${TIMESTAMP}.csv")

## Methodology

- Container started with default configuration
- 5-second warmup period
- 5 samples collected at 1-second intervals
- Average calculated from samples
- Container removed after measurement

## Environment

- OS: $(uname -s -r)
- Docker: $(docker --version)
- Architecture: $(uname -m)
EOF
    
    log_success "Report saved to: $RESULTS_DIR/BENCHMARK_REPORT_${TIMESTAMP}.md"
}

main "$@"
