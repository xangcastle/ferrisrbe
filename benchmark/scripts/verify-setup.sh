#!/bin/bash
# Verify Benchmark Setup
# Comprehensive check that all components are ready for benchmarking

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCHMARK_DIR="$(dirname "$SCRIPT_DIR")"
PROJECT_ROOT="$(dirname "$BENCHMARK_DIR")"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

PASS=0
FAIL=0
WARN=0

pass() {
    echo -e "${GREEN}✓${NC} $1"
    ((PASS++))
}

fail() {
    echo -e "${RED}✗${NC} $1"
    ((FAIL++))
}

warn() {
    echo -e "${YELLOW}⚠${NC} $1"
    ((WARN++))
}

info() {
    echo -e "${BLUE}ℹ${NC} $1"
}

header() {
    echo ""
    echo "========================================"
    echo "$1"
    echo "========================================"
}

# Check if command exists
check_command() {
    local cmd="$1"
    local required="${2:-true}"
    
    if command -v "$cmd" &> /dev/null; then
        local version=$("$cmd" --version 2>/dev/null | head -1 || echo "unknown")
        pass "$cmd: $version"
        return 0
    else
        if [ "$required" = "true" ]; then
            fail "$cmd: not found (required)"
            return 1
        else
            warn "$cmd: not found (optional)"
            return 1
        fi
    fi
}

# Check file exists and is executable
check_executable() {
    local file="$1"
    local desc="$2"
    
    if [ -f "$file" ]; then
        if [ -x "$file" ]; then
            pass "$desc"
            return 0
        else
            fail "$desc: not executable"
            return 1
        fi
    else
        fail "$desc: file not found"
        return 1
    fi
}

# Check directory exists
check_directory() {
    local dir="$1"
    local desc="$2"
    
    if [ -d "$dir" ]; then
        pass "$desc"
        return 0
    else
        fail "$desc: directory not found"
        return 1
    fi
}

# Main verification
main() {
    echo "========================================"
    echo "FerrisRBE Benchmark Setup Verification"
    echo "========================================"
    echo ""
    info "Project root: $PROJECT_ROOT"
    info "Benchmark dir: $BENCHMARK_DIR"
    
    # Section 1: Required Commands
    header "1. Required Commands"
    
    check_command "bazel" true || true
    check_command "python3" true || true
    check_command "docker" false || true
    check_command "bc" false || true
    
    # Section 2: Python Dependencies
    header "2. Python Dependencies"
    
    python3 -c "import grpc" 2>/dev/null && pass "grpc (Python)" || fail "grpc (Python): not installed"
    
    # Section 3: Directory Structure
    header "3. Directory Structure"
    
    check_directory "$BENCHMARK_DIR/scripts" "scripts/ directory"
    check_directory "$BENCHMARK_DIR/results" "results/ directory"
    check_directory "$BENCHMARK_DIR/config" "config/ directory"
    
    # Section 4: Core Scripts
    header "4. Core Benchmark Scripts"
    
    check_executable "$SCRIPT_DIR/bazel-utils.sh" "bazel-utils.sh"
    check_executable "$SCRIPT_DIR/build-with-bazel.sh" "build-with-bazel.sh"
    check_executable "$SCRIPT_DIR/benchmark.sh" "benchmark.sh"
    check_executable "$SCRIPT_DIR/benchmark-local.sh" "benchmark-local.sh (local testing)"
    check_executable "$SCRIPT_DIR/benchmark-ci.sh" "benchmark-ci.sh (CI/CD)"
    check_executable "$SCRIPT_DIR/check-regression.py" "check-regression.py"
    
    # Section 5: Test Scripts
    header "5. Test Scripts"
    
    check_executable "$SCRIPT_DIR/execution-load-test.py" "execution-load-test.py"
    check_executable "$SCRIPT_DIR/action-cache-test.py" "action-cache-test.py"
    check_executable "$SCRIPT_DIR/noisy-neighbor-test.py" "noisy-neighbor-test.py"
    check_executable "$SCRIPT_DIR/o1-streaming-test.py" "o1-streaming-test.py"
    check_executable "$SCRIPT_DIR/connection-churn-test.py" "connection-churn-test.py"
    check_executable "$SCRIPT_DIR/cache-stampede-test.py" "cache-stampede-test.py"
    check_executable "$SCRIPT_DIR/cold-start-test.sh" "cold-start-test.sh"
    
    # Section 6: Docker Compose Files
    header "6. Docker Compose Configurations"
    
    if [ -f "$BENCHMARK_DIR/docker-compose.ferrisrbe.yml" ]; then
        pass "docker-compose.ferrisrbe.yml"
    else
        fail "docker-compose.ferrisrbe.yml"
    fi
    
    if [ -f "$BENCHMARK_DIR/docker-compose.buildfarm.yml" ]; then
        pass "docker-compose.buildfarm.yml"
    else
        warn "docker-compose.buildfarm.yml (optional)"
    fi
    
    # Section 7: Bazel Configuration
    header "7. Bazel Configuration"
    
    if [ -f "$PROJECT_ROOT/.bazelrc" ]; then
        pass ".bazelrc exists"
        
        # Check for symlink_prefix configuration
        if grep -q "symlink_prefix" "$PROJECT_ROOT/.bazelrc" 2>/dev/null; then
            local prefix=$(grep "symlink_prefix" "$PROJECT_ROOT/.bazelrc" | head -1)
            info "Custom symlink_prefix detected: $prefix"
            info "Scripts will handle this automatically"
        fi
    else
        warn ".bazelrc: not found (using defaults)"
    fi
    
    if [ -f "$PROJECT_ROOT/MODULE.bazel" ]; then
        pass "MODULE.bazel (bzlmod)"
    elif [ -f "$PROJECT_ROOT/WORKSPACE" ]; then
        pass "WORKSPACE (legacy)"
    else
        fail "No Bazel workspace found"
    fi
    
    # Section 8: Test Bazel Functions
    header "8. Bazel Integration"
    
    if [ -f "$SCRIPT_DIR/bazel-utils.sh" ]; then
        source "$SCRIPT_DIR/bazel-utils.sh"
        
        # Test get_bazel_bin
        if command -v bazel &> /dev/null; then
            local bazel_bin=$(get_bazel_bin "$PROJECT_ROOT")
            if [ -n "$bazel_bin" ]; then
                pass "bazel info bazel-bin: $bazel_bin"
            else
                fail "Could not determine bazel-bin location"
            fi
            
            # Check if bazel-bin exists
            if [ -d "$bazel_bin" ]; then
                pass "bazel-bin directory exists"
            else
                warn "bazel-bin directory not yet created (run bazel build)"
            fi
        fi
    fi
    
    # Section 9: Build Test (Optional)
    header "9. Build Test (Optional)"
    
    if command -v bazel &> /dev/null; then
        info "Testing Bazel query..."
        if bazel query //:rbe-server &> /dev/null; then
            pass "Target //:rbe-server exists"
        else
            warn "Target //:rbe-server not found in BUILD files"
        fi
        
        # Check if already built
        local server_output=$(find_bazel_output "//:rbe-server" "$PROJECT_ROOT" 2>/dev/null || true)
        if [ -n "$server_output" ] && [ -f "$server_output" ]; then
            pass "rbe-server already built: $server_output"
        else
            info "rbe-server not yet built (run ./scripts/build-with-bazel.sh)"
        fi
    fi
    
    # Section 10: GitHub Actions (if applicable)
    header "10. CI/CD Configuration"
    
    if [ -f "$PROJECT_ROOT/.github/workflows/benchmark.yml" ]; then
        pass "GitHub Actions workflow: benchmark.yml"
    else
        warn "GitHub Actions workflow not found"
    fi
    
    # Summary
    header "Summary"
    
    echo "Passed: $PASS"
    echo "Warnings: $WARN"
    echo "Failed: $FAIL"
    echo ""
    
    if [ $FAIL -eq 0 ]; then
        echo -e "${GREEN}✅ Setup looks good!${NC}"
        echo ""
        echo "Next steps:"
        echo "  1. Build FerrisRBE: ./scripts/build-with-bazel.sh all"
        echo "  2. Run local test:  ./scripts/benchmark-local.sh light"
        echo "  3. Run benchmarks:  ./scripts/benchmark.sh"
        echo "  4. Run CI tests:    ./scripts/benchmark-ci.sh light"
        exit 0
    else
        echo -e "${RED}❌ Setup has issues. Please fix the failures above.${NC}"
        exit 1
    fi
}

# Run with optional flags
case "${1:-}" in
    --quick)
        # Quick mode - skip build tests
        info "Running in quick mode (skipping build tests)"
        main | grep -E "(✓|✗|⚠|ℹ|===)"
        ;;
    --help|-h)
        echo "Usage: $0 [--quick|--help]"
        echo ""
        echo "Options:"
        echo "  --quick  Skip build tests, faster check"
        echo "  --help   Show this help"
        echo ""
        echo "Verifies that the benchmark suite is properly configured."
        exit 0
        ;;
    *)
        main
        ;;
esac
