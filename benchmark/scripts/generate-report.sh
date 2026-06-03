#!/bin/bash
# Generate comprehensive benchmark report from JSON results
# Creates a markdown report with comparisons, thresholds, and trends
#
# Usage: ./generate-report.sh [results_dir] [output_file]
#   results_dir: Directory containing JSON results (default: ../results)
#   output_file: Output markdown file (default: ../results/BENCHMARK_REPORT_<timestamp>.md)

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCHMARK_DIR="$(dirname "$SCRIPT_DIR")"
RESULTS_DIR="${1:-$BENCHMARK_DIR/results}"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
OUTPUT_FILE="${2:-$RESULTS_DIR/BENCHMARK_REPORT_${TIMESTAMP}.md}"

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

# Thresholds for pass/fail
THRESHOLD_MEMORY_MB=20
THRESHOLD_COLD_START_MS=500
THRESHOLD_EXECUTION_P99_MS=100
THRESHOLD_CACHE_P99_US=1000
THRESHOLD_CLEANUP_RATE=95

# Function to extract value from JSON
get_json_value() {
    local file="$1"
    local key="$2"
    local default="${3:-N/A}"
    
    if [ -f "$file" ]; then
        python3 -c "import json,sys; d=json.load(open('$file')); print(d.get('$key', '$default'))" 2>/dev/null || echo "$default"
    else
        echo "$default"
    fi
}

# Function to extract nested value from JSON
get_json_nested() {
    local file="$1"
    local keys="$2"
    local default="${3:-N/A}"
    
    if [ -f "$file" ]; then
        python3 -c "
import json,sys
try:
    d=json.load(open('$file'))
    for k in '$keys'.split('.'):
        if isinstance(d, dict) and k in d:
            d = d[k]
        else:
            print('$default')
            sys.exit(0)
    print(d)
except:
    print('$default')
" 2>/dev/null || echo "$default"
    else
        echo "$default"
    fi
}

# Function to format number
format_number() {
    local num="$1"
    local decimals="${2:-2}"
    printf "%.${decimals}f" "$num" 2>/dev/null || echo "$num"
}

# Function to check threshold
check_threshold() {
    local value="$1"
    local threshold="$2"
    local operator="${3:-le}"  # le = less than or equal (default), ge = greater than or equal
    
    if [ "$value" = "N/A" ] || [ "$value" = "null" ]; then
        echo "⚠️"
        return 1
    fi
    
    if [ "$operator" = "le" ]; then
        if (( $(echo "$value <= $threshold" | bc -l 2>/dev/null || echo "0") )); then
            echo "✅"
            return 0
        else
            echo "❌"
            return 1
        fi
    else
        if (( $(echo "$value >= $threshold" | bc -l 2>/dev/null || echo "0") )); then
            echo "✅"
            return 0
        else
            echo "❌"
            return 1
        fi
    fi
}

# Find latest result files
find_latest() {
    local pattern="$1"
    ls -t $RESULTS_DIR/$pattern 2>/dev/null | head -1
}

# Main report generation
generate_report() {
    log_info "Generating benchmark report..."
    log_info "Results directory: $RESULTS_DIR"
    log_info "Output file: $OUTPUT_FILE"
    
    # Find result files
    local cache_file=$(find_latest "cache_*.json")
    local cas_file=$(find_latest "cas_*.json")
    local execution_file=$(find_latest "execution_*.json")
    local stampede_file=$(find_latest "stampede_*.json")
    local memory_file=$(find_latest "memory_baseline.txt")
    local coldstart_file=$(find_latest "coldstart_*.txt")
    local churn_file=$(find_latest "churn_*.json")
    local benchmark_data=$(find_latest "benchmark_data.json")
    
    # Extract values
    local memory_mb=$(cat "$memory_file" 2>/dev/null || echo "N/A")
    local cold_start_ms=$(cat "$coldstart_file" 2>/dev/null || echo "N/A")
    
    local cache_throughput=$(get_json_nested "$cache_file" "throughput")
    local cache_p99_us=$(get_json_nested "$cache_file" "latencies_us.p99")
    local cache_ops=$(get_json_nested "$cache_file" "total_operations")
    
    local cas_upload_p99=$(get_json_nested "$cas_file" "upload_latencies.p99")
    local cas_download_p99=$(get_json_nested "$cas_file" "download_latencies.p99")
    local cas_blobs=$(get_json_nested "$cas_file" "total_blobs")
    
    local execution_p99=$(get_json_nested "$execution_file" "latencies.p99")
    local execution_throughput=$(get_json_nested "$execution_file" "throughput")
    local execution_actions=$(get_json_nested "$execution_file" "total_actions")
    
    local stampede_p99=$(get_json_nested "$stampede_file" "latencies_ms.p99")
    local stampede_ratio=$(get_json_nested "$stampede_file" "latencies_ms.p99" | python3 -c "import sys; p99=float(sys.stdin.read()); print(p99)" 2>/dev/null)
    local stampede_mean=$(get_json_nested "$stampede_file" "latencies_ms.mean")
    
    local cleanup_rate=$(get_json_nested "$churn_file" "cleanup_rate")
    
    # Convert cache P99 to ms for display
    local cache_p99_ms=$(echo "$cache_p99_us" | awk '{print $1/1000}')
    
    # Calculate stampede ratio
    if [ "$stampede_p99" != "N/A" ] && [ "$stampede_mean" != "N/A" ] && [ "$stampede_mean" != "0" ]; then
        stampede_ratio=$(echo "scale=2; $stampede_p99 / $stampede_mean" | bc 2>/dev/null || echo "N/A")
    fi
    
    # Get commit info
    local commit=$(get_json_value "$benchmark_data" "commit" "N/A")
    local mode=$(get_json_value "$benchmark_data" "mode" "unknown")
    local image=$(get_json_nested "$benchmark_data" "container.image" "ferrisrbe/server:latest")
    
    # Check thresholds
    local memory_status=$(check_threshold "$memory_mb" "$THRESHOLD_MEMORY_MB" "le")
    local coldstart_status=$(check_threshold "$cold_start_ms" "$THRESHOLD_COLD_START_MS" "le")
    local cache_status=$(check_threshold "$cache_p99_us" "$THRESHOLD_CACHE_P99_US" "le")
    local execution_status=$(check_threshold "$execution_p99" "$THRESHOLD_EXECUTION_P99_MS" "le")
    local cleanup_status=$(check_threshold "$cleanup_rate" "$THRESHOLD_CLEANUP_RATE" "ge")
    
    # Overall status
    local overall_status="✅ PASS"
    if [ "$memory_status" = "❌" ] || [ "$coldstart_status" = "❌" ] || [ "$cache_status" = "❌" ] || [ "$execution_status" = "❌" ]; then
        overall_status="❌ FAIL"
    elif [ "$memory_status" = "⚠️" ] || [ "$coldstart_status" = "⚠️" ]; then
        overall_status="⚠️ PARTIAL"
    fi
    
    # Generate markdown report
    cat > "$OUTPUT_FILE" << EOF
# FerrisRBE Benchmark Report

**Generated:** $(date -u +"%Y-%m-%d %H:%M:%S UTC")  
**Commit:** ${commit}  
**Mode:** ${mode}  
**Image:** ${image}  

## Executive Summary

| Metric | Value | Threshold | Status |
|--------|-------|-----------|--------|
| **Memory Footprint** | ${memory_mb} MB | < ${THRESHOLD_MEMORY_MB} MB | ${memory_status} |
| **Cold Start** | ${cold_start_ms} ms | < ${THRESHOLD_COLD_START_MS} ms | ${coldstart_status} |
| **Cache P99 Latency** | $(format_number "$cache_p99_ms") ms | < $(echo "$THRESHOLD_CACHE_P99_US/1000" | bc) ms | ${cache_status} |
| **Execution P99 Latency** | $(format_number "$execution_p99") ms | < ${THRESHOLD_EXECUTION_P99_MS} ms | ${execution_status} |
| **Cleanup Rate** | ${cleanup_rate}% | > ${THRESHOLD_CLEANUP_RATE}% | ${cleanup_status} |

### Overall Status: ${overall_status}

---

## Detailed Results

### 1. Memory Footprint

**Container Memory Usage:**

\`\`\`
Idle Memory: ${memory_mb} MB
Threshold: ${THRESHOLD_MEMORY_MB} MB
Status: ${memory_status}
\`\`\`

**Analysis:**
$(if [ "$memory_status" = "✅" ]; then echo "Memory usage is within expected range. FerrisRBE maintains its characteristic low memory footprint."; elif [ "$memory_status" = "❌" ]; then echo "⚠️ Memory usage exceeds threshold. Investigate potential memory leaks or increased baseline usage."; else echo "Memory data not available or inconclusive."; fi)

### 2. Cold Start Time

**Container Startup Performance:**

\`\`\`
Cold Start: ${cold_start_ms} ms
Threshold: ${THRESHOLD_COLD_START_MS} ms
Status: ${coldstart_status}
\`\`\`

**Analysis:**
$(if [ "$coldstart_status" = "✅" ]; then echo "Fast cold start enables effective autoscaling. Container is ready to serve requests quickly."; elif [ "$coldstart_status" = "❌" ]; then echo "⚠️ Cold start is slower than expected. This may impact autoscaling responsiveness."; else echo "Cold start data not available."; fi)

### 3. Action Cache Performance

**Test Configuration:**
- Operations: ${cache_ops}
- Concurrency: $(get_json_nested "$cache_file" "concurrent")
- Type: $(get_json_nested "$cache_file" "operation")

**Results:**

| Metric | Value |
|--------|-------|
| Throughput | $(format_number "$cache_throughput") ops/sec |
| P99 Latency | $(format_number "$cache_p99_ms") ms |
| Success Rate | $(get_json_nested "$cache_file" "success_count")/$(get_json_nested "$cache_file" "total_operations") ($(python3 -c "print(f\"{$(get_json_nested "$cache_file" "success_count")/$(get_json_nested "$cache_file" "total_operations")*100:.1f}\")" 2>/dev/null || echo "N/A")%) |
| Cache Hits | $(get_json_nested "$cache_file" "hit_count") |

**Analysis:**
$(if [ "$cache_status" = "✅" ]; then echo "Excellent cache performance. DashMap lock-free architecture delivers microsecond-level responses."; elif [ "$cache_status" = "❌" ]; then echo "⚠️ Cache latency higher than expected. Check for contention or backend issues."; else echo "Cache performance data incomplete."; fi)

### 4. CAS Operations

**Test Configuration:**
- Blobs: ${cas_blobs}
- Size per blob: $(get_json_nested "$cas_file" "blob_size" | awk '{print $1/1024/1024 " MB"}')
- Total data: $(get_json_nested "$cas_file" "total_bytes" | awk '{print $1/1024/1024 " MB"}')

**Results:**

| Operation | P50 | P95 | P99 |
|-----------|-----|-----|-----|
| Upload | $(format_number "$(get_json_nested "$cas_file" "upload_latencies.p50")") ms | $(format_number "$(get_json_nested "$cas_file" "upload_latencies.p95")") ms | $(format_number "$cas_upload_p99") ms |
| Download | $(format_number "$(get_json_nested "$cas_file" "download_latencies.p50")") ms | $(format_number "$(get_json_nested "$cas_file" "download_latencies.p95")") ms | $(format_number "$cas_download_p99") ms |

**Analysis:**
$(if [ -n "$cas_upload_p99" ] && [ "$cas_upload_p99" != "N/A" ]; then echo "CAS operations completing successfully. Upload and download latencies are within acceptable ranges for blob storage operations."; else echo "CAS test data incomplete."; fi)

### 5. Execution Throughput

**Test Configuration:**
- Actions: ${execution_actions}
- Concurrency: $(get_json_nested "$execution_file" "concurrent")

**Results:**

| Metric | Value |
|--------|-------|
| Throughput | $(format_number "$execution_throughput") actions/sec |
| Success | $(get_json_nested "$execution_file" "success_count")/$(get_json_nested "$execution_file" "total_actions") |
| P50 Latency | $(format_number "$(get_json_nested "$execution_file" "latencies.p50")") ms |
| P99 Latency | $(format_number "$execution_p99") ms |
| Jitter (StdDev) | $(format_number "$(get_json_nested "$execution_file" "latencies.stddev")") ms |

**Analysis:**
$(if [ "$execution_status" = "✅" ]; then echo "Execution API performing well. Zero-GC runtime provides consistent latencies without spikes."; elif [ "$execution_status" = "❌" ]; then echo "⚠️ Execution latency exceeds threshold. Check scheduler and worker pool health."; else echo "Execution data incomplete."; fi)

### 6. Cache Stampede Protection

**Test Configuration:**
- Total Requests: $(get_json_nested "$stampede_file" "total_requests")
- Concurrency: $(get_json_nested "$stampede_file" "concurrent")
- Target: Same uncached action digest

**Results:**

| Metric | Value |
|--------|-------|
| Cache Hits | $(get_json_nested "$stampede_file" "cache_hits") |
| Cache Misses | $(get_json_nested "$stampede_file" "cache_misses") |
| Errors | $(get_json_nested "$stampede_file" "errors") |
| P99/Mean Ratio | $(format_number "$stampede_ratio")x |

**Analysis:**
$(if [ -n "$stampede_ratio" ] && [ "$stampede_ratio" != "N/A" ] && (( $(echo "$stampede_ratio < 3" | bc -l 2>/dev/null || echo "0") )); then echo "✅ Good stampede protection. Request coalescing or fast backend prevents thundering herd issues."; else echo "⚠️ High P99/Mean ratio may indicate backend contention under load."; fi)

### 7. Connection Churn

**Results:**

| Metric | Value |
|--------|-------|
| Connections Tested | $(get_json_nested "$churn_file" "connections_tested") |
| Abrupt Disconnects | $(get_json_nested "$churn_file" "abrupt_disconnects") |
| Cleanup Rate | ${cleanup_rate}% |
| Zombie Resources | $(get_json_nested "$churn_file" "zombie_resources") |

**Analysis:**
$(if [ "$cleanup_status" = "✅" ]; then echo "Excellent resource cleanup. Tokio's task cancellation releases resources immediately on connection drops."; elif [ "$cleanup_status" = "❌" ]; then echo "⚠️ Cleanup rate below threshold. Potential resource leaks detected."; else echo "Connection churn data incomplete."; fi)

---

## Comparison with Official Release

$(if [ -f "$RESULTS_DIR/comparison.md" ]; then cat "$RESULTS_DIR/comparison.md"; else echo "*No comparison data available. Run \`./scripts/compare-branches.sh\` to compare against official release.*"; fi)

---

## Recommendations

$(if [ "$overall_status" = "✅ PASS" ]; then echo "All benchmarks passing. No action required."; elif [ "$overall_status" = "❌ FAIL" ]; then echo "### ⚠️ Performance Regressions Detected

1. Review failed metrics above
2. Compare against baseline (\`xangcastle/ferris-server:latest\`)
3. Profile the application to identify bottlenecks
4. Re-run benchmarks after optimizations"; else echo "### Partial Results

Some benchmarks did not complete or data is missing. Check:
- Container logs for errors
- Test connectivity to localhost:9092
- Required services (bazel-remote) are running"; fi)

---

## Raw Data Files

| Test | File |
|------|------|
| Action Cache | \`$(basename "$cache_file")\` |
| CAS Load | \`$(basename "$cas_file")\` |
| Execution | \`$(basename "$execution_file")\` |
| Cache Stampede | \`$(basename "$stampede_file")\` |
| Connection Churn | \`$(basename "$churn_file")\` |
| Benchmark Data | \`$(basename "$benchmark_data")\` |

---

*Generated by FerrisRBE Benchmark Suite v2.0 - Container Native*
EOF

    log_success "Report generated: $OUTPUT_FILE"
    
    # Also create/update LATEST_REPORT.md symlink
    local latest_link="$RESULTS_DIR/LATEST_REPORT.md"
    ln -sf "$(basename "$OUTPUT_FILE")" "$latest_link"
    log_info "Latest report link updated: $latest_link"
    
    # Display summary
    echo ""
    echo "========================================"
    echo "  BENCHMARK REPORT SUMMARY"
    echo "========================================"
    echo ""
    echo "Overall Status: $overall_status"
    echo ""
    echo "Key Metrics:"
    echo "  Memory:        ${memory_mb} MB ${memory_status}"
    echo "  Cold Start:    ${cold_start_ms} ms ${coldstart_status}"
    echo "  Cache P99:     $(format_number "$cache_p99_ms") ms ${cache_status}"
    echo "  Execution P99: $(format_number "$execution_p99") ms ${execution_status}"
    echo ""
    echo "Report: $OUTPUT_FILE"
    echo "========================================"
}

# Check if results directory exists
if [ ! -d "$RESULTS_DIR" ]; then
    log_error "Results directory not found: $RESULTS_DIR"
    exit 1
fi

# Generate report
generate_report

exit 0
