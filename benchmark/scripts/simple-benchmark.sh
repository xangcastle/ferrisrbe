#!/bin/bash
# Simple RBE Benchmark Script
# Uses docker stats and grpcurl/curl for load generation

set -e

TARGET="${1:-ferrisrbe}"
DURATION="${2:-60}"
RESULTS_DIR="${3:-./results}"

mkdir -p "$RESULTS_DIR"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
RESULT_FILE="$RESULTS_DIR/${TARGET}_${TIMESTAMP}.txt"

echo "========================================" | tee -a "$RESULT_FILE"
echo "RBE Benchmark: $TARGET" | tee -a "$RESULT_FILE"
echo "Timestamp: $TIMESTAMP" | tee -a "$RESULT_FILE"
echo "Duration: ${DURATION}s" | tee -a "$RESULT_FILE"
echo "========================================" | tee -a "$RESULT_FILE"
echo "" | tee -a "$RESULT_FILE"

# Determine container name based on target
case "$TARGET" in
    ferrisrbe)
        CONTAINER="ferrisrbe-server"
        PORT=9092
        ;;
    buildfarm)
        CONTAINER="buildfarm-server"
        PORT=9092
        ;;
    buildbarn)
        CONTAINER="buildbarn-frontend"
        PORT=9092
        ;;
    buildbuddy)
        CONTAINER="buildbuddy-server"
        PORT=9092
        ;;
    *)
        echo "Unknown target: $TARGET"
        exit 1
        ;;
esac

echo "📊 Baseline metrics (idle):" | tee -a "$RESULT_FILE"
docker stats "$CONTAINER" --no-stream --format "Memory: {{.MemUsage}} | CPU: {{.CPUPerc}}" 2>/dev/null | tee -a "$RESULT_FILE" || echo "Container not running" | tee -a "$RESULT_FILE"

echo "" | tee -a "$RESULT_FILE"
echo "🚀 Starting load generation for ${DURATION}s..." | tee -a "$RESULT_FILE"

# Collect metrics in background
METRICS_FILE=$(mktemp)
echo "timestamp,container,memory_mb,memory_percent,cpu_percent" > "$METRICS_FILE"

# Function to collect metrics
collect_metrics() {
    local start_time=$(date +%s)
    while true; do
        local current_time=$(date +%s)
        local elapsed=$((current_time - start_time))
        if [ $elapsed -ge $DURATION ]; then
            break
        fi
        
        # Get stats
        STATS=$(docker stats "$CONTAINER" --no-stream --format "{{.MemUsage}},{{.MemPerc}},{{.CPUPerc}}" 2>/dev/null)
        if [ -n "$STATS" ]; then
            # Parse memory usage (e.g., "50MiB / 512MiB" -> 50)
            MEM_RAW=$(echo "$STATS" | cut -d',' -f1 | awk '{print $1}')
            MEM_MB=$(echo "$MEM_RAW" | sed 's/MiB//' | sed 's/GiB/*1024/' | sed 's/KiB\/1024/' | bc 2>/dev/null || echo "0")
            MEM_PCT=$(echo "$STATS" | cut -d',' -f2 | sed 's/%//')
            CPU_PCT=$(echo "$STATS" | cut -d',' -f3 | sed 's/%//')
            
            echo "$(date +%s),$CONTAINER,$MEM_MB,$MEM_PCT,$CPU_PCT" >> "$METRICS_FILE"
        fi
        
        # Generate load using grpcurl if available
        if command -v grpcurl &> /dev/null; then
            # Small gRPC request
            grpcurl -plaintext -max-time 2 "localhost:$PORT" build.bazel.remote.execution.v2.Capabilities/GetCapabilities 2>/dev/null > /dev/null || true
        fi
        
        sleep 1
    done
}

# Start collecting metrics in background
collect_metrics &
METRICS_PID=$!

# Wait for completion
wait $METRICS_PID

echo "" | tee -a "$RESULT_FILE"
echo "📊 Results Summary:" | tee -a "$RESULT_FILE"
echo "-------------------" | tee -a "$RESULT_FILE"

# Calculate statistics from collected metrics
if [ -f "$METRICS_FILE" ] && [ $(wc -l < "$METRICS_FILE") -gt 1 ]; then
    # Skip header and calculate stats
    echo "Memory (MB) Statistics:" | tee -a "$RESULT_FILE"
    echo "  Min: $(tail -n +2 "$METRICS_FILE" | cut -d',' -f3 | sort -n | head -1)" | tee -a "$RESULT_FILE"
    echo "  Max: $(tail -n +2 "$METRICS_FILE" | cut -d',' -f3 | sort -n | tail -1)" | tee -a "$RESULT_FILE"
    echo "  Avg: $(tail -n +2 "$METRICS_FILE" | cut -d',' -f3 | awk '{sum+=$1; count++} END {printf "%.1f", sum/count}')" | tee -a "$RESULT_FILE"
    
    echo "" | tee -a "$RESULT_FILE"
    echo "CPU (%) Statistics:" | tee -a "$RESULT_FILE"
    echo "  Min: $(tail -n +2 "$METRICS_FILE" | cut -d',' -f5 | sort -n | head -1)" | tee -a "$RESULT_FILE"
    echo "  Max: $(tail -n +2 "$METRICS_FILE" | cut -d',' -f5 | sort -n | tail -1)" | tee -a "$RESULT_FILE"
    echo "  Avg: $(tail -n +2 "$METRICS_FILE" | cut -d',' -f5 | awk '{sum+=$1; count++} END {printf "%.1f", sum/count}')" | tee -a "$RESULT_FILE"
    
    # Save metrics to result dir
    cp "$METRICS_FILE" "$RESULTS_DIR/${TARGET}_${TIMESTAMP}_metrics.csv"
    echo "" | tee -a "$RESULT_FILE"
    echo "📁 Raw metrics saved to: $RESULTS_DIR/${TARGET}_${TIMESTAMP}_metrics.csv" | tee -a "$RESULT_FILE"
fi

# Cleanup
rm -f "$METRICS_FILE"

echo "" | tee -a "$RESULT_FILE"
echo "✅ Benchmark complete!" | tee -a "$RESULT_FILE"
