#!/bin/bash
# Compare benchmark results between two branches/commits
# Usage: ./compare-branches.sh <main-binary> <pr-binary>

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCHMARK_DIR="$(dirname "$SCRIPT_DIR")"
RESULTS_DIR="$BENCHMARK_DIR/results"

MAIN_BINARY="${1:-}"
PR_BINARY="${2:-}"

if [ -z "$MAIN_BINARY" ] || [ -z "$PR_BINARY" ]; then
    echo "Usage: $0 <main-binary> <pr-binary>"
    echo "Example: $0 ./main/rbe-server ./pr/rbe-server"
    exit 1
fi

# Verify binaries exist
if [ ! -f "$MAIN_BINARY" ]; then
    echo "ERROR: Main binary not found: $MAIN_BINARY"
    exit 1
fi

if [ ! -f "$PR_BINARY" ]; then
    echo "ERROR: PR binary not found: $PR_BINARY"
    exit 1
fi

mkdir -p "$RESULTS_DIR"

echo "========================================"
echo "Branch Comparison Benchmark"
echo "========================================"
echo "Main:  $MAIN_BINARY"
echo "PR:    $PR_BINARY"
echo "========================================"
echo ""

# Wait for bazel-remote if using GitHub Actions services
if [ -n "$BENCHMARK_SERVICES" ]; then
    echo "Waiting for bazel-remote service..."
    for i in {1..60}; do
        if nc -z localhost 9094 2>/dev/null; then
            echo "✓ bazel-remote is ready on port 9094"
            break
        fi
        echo "  Waiting... (attempt $i/60)"
        sleep 2
    done
fi

# Ensure CAS_ENDPOINT is set
if [ -z "$CAS_ENDPOINT" ]; then
    export CAS_ENDPOINT="localhost:9094"
fi

# Function to benchmark a binary
benchmark_binary() {
    local binary="$1"
    local name="$2"
    local output_dir="$RESULTS_DIR/${name}"
    
    mkdir -p "$output_dir"
    
    echo "Benchmarking $name..."
    
    # Set CAS_ENDPOINT if provided via environment
    if [ -n "$CAS_ENDPOINT" ]; then
        export CAS_ENDPOINT
        echo "  Using CAS_ENDPOINT: $CAS_ENDPOINT"
    fi
    
    # Start server
    "$binary" &
    local pid=$!
    
    # Wait for server
    for i in {1..60}; do
        if nc -z localhost 9092 2>/dev/null; then
            break
        fi
        sleep 1
    done
    
    if ! nc -z localhost 9092 2>/dev/null; then
        echo "ERROR: $name server failed to start"
        kill $pid 2>/dev/null || true
        return 1
    fi
    
    # Get memory baseline (multiple samples like benchmark-ci.sh)
    echo "  Sampling memory..."
    sleep 2
    for i in {1..5}; do
        ps -o rss= -p $pid 2>/dev/null | awk '{print $1/1024}' || echo "0"
        sleep 1
    done > "$output_dir/memory_samples.txt"
    
    # Use first non-zero value (consistent with benchmark-ci.sh)
    MEMORY=$(grep -v "^0$" "$output_dir/memory_samples.txt" | head -1)
    [ -z "$MEMORY" ] && MEMORY="0"
    
    echo "$MEMORY" > "$output_dir/memory.txt"
    echo "  Memory: ${MEMORY}MB (avg of samples)"
    
    # Quick throughput test
    python3 "$SCRIPT_DIR/execution-load-test.py" \
        --server localhost:9092 \
        --actions 50 \
        --concurrent 10 \
        --output "$output_dir/execution.json" 2>/dev/null || true
    
    # Stop server
    kill $pid 2>/dev/null || true
    wait $pid 2>/dev/null || true
    
    sleep 2
}

# Benchmark main branch
echo "📊 Benchmarking MAIN branch..."
benchmark_binary "$MAIN_BINARY" "main"

# Benchmark PR branch
echo "📊 Benchmarking PR branch..."
benchmark_binary "$PR_BINARY" "pr"

# Generate comparison report
echo ""
echo "Generating comparison report..."

COMPARISON_FILE="$RESULTS_DIR/comparison.md"

cat > "$COMPARISON_FILE" << EOF
### Performance Comparison: PR vs Main

| Metric | Main | PR | Change | Status |
|--------|------|-----|--------|--------|
EOF

# Compare memory
if [ -f "$RESULTS_DIR/main/memory.txt" ] && [ -f "$RESULTS_DIR/pr/memory.txt" ]; then
    MAIN_MEM=$(cat "$RESULTS_DIR/main/memory.txt")
    PR_MEM=$(cat "$RESULTS_DIR/pr/memory.txt")
    
    # Calculate change
    if (( $(echo "$MAIN_MEM > 0" | bc -l) )); then
        CHANGE=$(echo "scale=2; (($PR_MEM - $MAIN_MEM) / $MAIN_MEM) * 100" | bc)
    else
        CHANGE="0"
    fi
    
    # Determine status
    if (( $(echo "$CHANGE <= 5" | bc -l) )); then
        STATUS="✅"
    elif (( $(echo "$CHANGE <= 15" | bc -l) )); then
        STATUS="⚠️"
    else
        STATUS="🚨"
    fi
    
    ARROW=$(echo "$CHANGE" | awk '{if ($1 < 0) print "↓"; else if ($1 > 0) print "↑"; else print "="}')
    
    echo "| Memory (MB) | ${MAIN_MEM} | ${PR_MEM} | ${ARROW} ${CHANGE}% | ${STATUS} |" >> "$COMPARISON_FILE"
fi

cat >> "$COMPARISON_FILE" << EOF

#### Legend
- ✅ Within 5% - Acceptable
- ⚠️ 5-15% change - Review recommended
- 🚨 >15% regression - Optimization required

#### Interpretation
EOF

# Add interpretation based on results
if [ -f "$RESULTS_DIR/pr/memory.txt" ]; then
    PR_MEM=$(cat "$RESULTS_DIR/pr/memory.txt")
    if (( $(echo "$PR_MEM > 20" | bc -l) )); then
        echo "- 🚨 **Memory regression detected**: PR uses ${PR_MEM}MB vs expected <20MB" >> "$COMPARISON_FILE"
    else
        echo "- ✅ **Memory usage acceptable**: ${PR_MEM}MB within expected range" >> "$COMPARISON_FILE"
    fi
fi

echo "- Comparison generated at: $(date -u +"%Y-%m-%d %H:%M:%S UTC")" >> "$COMPARISON_FILE"

echo ""
echo "========================================"
echo "Comparison complete!"
echo "Report: $COMPARISON_FILE"
echo "========================================"

# Display the comparison
cat "$COMPARISON_FILE"
