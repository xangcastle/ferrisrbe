#!/bin/bash
# Cold Start Test for RBE Servers
# Measures time from container start to first successful gRPC response

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
log_error() { echo -e "${RED}[ERROR]${NC} $1"; }

# Test cold start for a solution
test_cold_start() {
    local name="$1"
    local image="$2"
    local port="$3"
    local env_vars="${4:-}"
    local timeout_secs="${5:-60}"
    
    log_info "Testing cold start for $name..."
    
    local container_name="coldstart-$(echo "$name" | tr '[:upper:]' '[:lower:]')-${TIMESTAMP}"
    
    # Record start time
    local start_time=$(date +%s%N)
    
    # Start container
    local run_cmd="docker run -d --name $container_name -p $port:$port"
    if [ -n "$env_vars" ]; then
        run_cmd="$run_cmd $env_vars"
    fi
    run_cmd="$run_cmd $image"
    
    if ! eval "$run_cmd" 2>/dev/null; then
        log_error "Failed to start $name container"
        echo "$name,N/A,failed" >> "$RESULTS_DIR/coldstart_${TIMESTAMP}.csv"
        return 1
    fi
    
    # Wait for port to be listening
    local port_ready=false
    local port_attempts=0
    local max_port_attempts=$timeout_secs
    
    while [ $port_attempts -lt $max_port_attempts ]; do
        if nc -z localhost $port 2>/dev/null; then
            port_ready=true
            break
        fi
        sleep 0.1
        port_attempts=$((port_attempts + 1))
    done
    
    if [ "$port_ready" = false ]; then
        log_error "Port $port never became ready"
        docker rm -f "$container_name" >/dev/null 2>&1
        echo "$name,N/A,timeout" >> "$RESULTS_DIR/coldstart_${TIMESTAMP}.csv"
        return 1
    fi
    
    local port_ready_time=$(date +%s%N)
    
    # Wait for first successful gRPC response
    local grpc_ready=false
    local grpc_attempts=0
    local max_grpc_attempts=30  # 3 seconds max
    
    while [ $grpc_attempts -lt $max_grpc_attempts ]; do
        if grpcurl -plaintext -max-time 1 "localhost:$port" \
            build.bazel.remote.execution.v2.Capabilities/GetCapabilities 2>/dev/null; then
            grpc_ready=true
            break
        fi
        sleep 0.1
        grpc_attempts=$((grpc_attempts + 1))
    done
    
    local end_time=$(date +%s%N)
    
    # Calculate times in milliseconds
    local total_ms=$(( (end_time - start_time) / 1000000 ))
    local port_ms=$(( (port_ready_time - start_time) / 1000000 ))
    local grpc_ms=$(( (end_time - port_ready_time) / 1000000 ))
    
    # Cleanup
    docker rm -f "$container_name" >/dev/null 2>&1
    
    if [ "$grpc_ready" = true ]; then
        log_success "$name: ${total_ms}ms total (port: ${port_ms}ms, grpc: ${grpc_ms}ms)"
        echo "$name,$total_ms,$port_ms,$grpc_ms,success" >> "$RESULTS_DIR/coldstart_${TIMESTAMP}.csv"
    else
        log_warn "$name: Port ready in ${port_ms}ms but gRPC failed"
        echo "$name,$port_ms,$port_ms,N/A,grpc_failed" >> "$RESULTS_DIR/coldstart_${TIMESTAMP}.csv"
    fi
}

# Main test
main() {
    echo "========================================"
    echo "RBE Cold Start Test"
    echo "Timestamp: $TIMESTAMP"
    echo "========================================"
    echo ""
    
    # Initialize CSV
    echo "solution,total_ms,port_ready_ms,grpc_ready_ms,status" > "$RESULTS_DIR/coldstart_${TIMESTAMP}.csv"
    
    # Check if FerrisRBE image exists
    if docker image inspect ferrisrbe-server:latest >/dev/null 2>&1; then
        test_cold_start \
            "FerrisRBE" \
            "ferrisrbe-server:latest" \
            9092 \
            "-e RBE_PORT=9092 -e RBE_BIND_ADDRESS=0.0.0.0"
    else
        log_warn "FerrisRBE image not found. Build it first with Bazel:"
        echo "  bazel build //oci:server_image"
        echo "  bazel run //oci:server_load"
    fi
    
    # Test other solutions if available
    if docker image inspect bazelbuild/buildfarm-server:latest >/dev/null 2>&1; then
        echo ""
        test_cold_start \
            "Buildfarm" \
            "bazelbuild/buildfarm-server:latest" \
            9092 \
            "-e JAVA_OPTS=-Xmx2g -Xms1g"
    fi
    
    # Generate report
    echo ""
    echo "========================================"
    echo "Cold Start Results"
    echo "========================================"
    column -s',' -t "$RESULTS_DIR/coldstart_${TIMESTAMP}.csv"
    echo ""
    echo "Full results saved to: $RESULTS_DIR/coldstart_${TIMESTAMP}.csv"
    
    # Generate markdown report
    cat > "$RESULTS_DIR/COLDSTART_REPORT_${TIMESTAMP}.md" << EOF
# RBE Cold Start Test Report

**Date:** $(date -u +"%Y-%m-%d %H:%M:%S UTC")  
**Runner:** $(whoami)@$(hostname)

## Results

$(column -s',' -t "$RESULTS_DIR/coldstart_${TIMESTAMP}.csv")

## Interpretation

- **Total Time**: Time from 'docker run' to first successful gRPC response
- **Port Ready**: Time until TCP port is listening
- **gRPC Ready**: Additional time until first gRPC request succeeds

### Expected Results

| Solution | Typical Cold Start | Notes |
|----------|-------------------|-------|
| FerrisRBE | < 100ms | Rust native binary, distroless |
| Buildfarm | 5-15s | JVM startup + JIT warmup |
| Buildbarn | 1-3s | Go binary |
| BuildBuddy | 10-30s | JVM + PostgreSQL + Redis |

Fast cold start is critical for:
- Kubernetes HPA (Horizontal Pod Autoscaler) responsiveness
- Serverless deployments
- Development/testing environments

## Methodology

1. Start container with 'docker run'
2. Poll TCP port until listening
3. Send gRPC Capabilities request
4. Measure total time
EOF
    
    log_success "Report saved to: $RESULTS_DIR/COLDSTART_REPORT_${TIMESTAMP}.md"
}

main "$@"
