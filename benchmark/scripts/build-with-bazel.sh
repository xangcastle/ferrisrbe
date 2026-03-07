#!/bin/bash
# Build FerrisRBE using Bazel (dogfooding)
# This script builds the RBE server using Bazel, consistent with the project's build system
# Handles custom symlink prefixes (--symlink_prefix=/)

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$(dirname "$SCRIPT_DIR")")"

cd "$PROJECT_ROOT"

# Source Bazel utilities
source "$SCRIPT_DIR/bazel-utils.sh"

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

# Check Bazel installation
check_bazel_installed() {
    if ! command -v bazel &> /dev/null; then
        log_error "Bazel not found. Please install Bazelisk:"
        echo "  npm install -g @bazel/bazelisk"
        echo "  or"
        echo "  brew install bazelisk"
        exit 1
    fi
    
    local bazel_version=$(bazel --version | grep -oE '[0-9]+\.[0-9]+\.[0-9]+')
    log_info "Using Bazel version: $bazel_version"
    
    # Show bazel-bin location (handles custom symlink_prefix)
    local bazel_bin=$(get_bazel_bin)
    log_info "Bazel output directory: $bazel_bin"
}

# Build server binary
build_server() {
    log_info "Building rbe-server with Bazel..."
    
    # Build
    bazel build //:rbe-server --config=release 2>/dev/null || bazel build //:rbe-server
    
    # Find output (handles custom symlink prefix)
    local output_path=$(find_bazel_output "//:rbe-server")
    
    if [ -z "$output_path" ] || [ ! -f "$output_path" ]; then
        log_error "Could not find rbe-server output"
        log_info "Tried: $(get_bazel_bin)/rbe-server"
        exit 1
    fi
    
    log_success "Server binary built: $output_path"
    echo "$output_path" > "$PROJECT_ROOT/.bazel-output-server"
}

# Build OCI image
build_image() {
    log_info "Building OCI image with Bazel (rules_oci)..."
    
    # Build the image
    bazel build //oci:server_image
    
    # Load into Docker for benchmarking
    bazel run //oci:server_load
    
    # Tag for easier reference
    docker tag bazel/oci:server_image ferrisrbe-server:latest 2>/dev/null || true
    
    log_success "OCI image built and loaded: ferrisrbe-server:latest"
}

# Build worker binary
build_worker() {
    log_info "Building rbe-worker with Bazel..."
    
    bazel build //:rbe-worker --config=release 2>/dev/null || bazel build //:rbe-worker
    
    local output_path=$(find_bazel_output "//:rbe-worker")
    
    if [ -z "$output_path" ] || [ ! -f "$output_path" ]; then
        log_error "Could not find rbe-worker output"
        exit 1
    fi
    
    log_success "Worker binary built: $output_path"
    echo "$output_path" > "$PROJECT_ROOT/.bazel-output-worker"
}

# Run a quick smoke test
smoke_test() {
    log_info "Running smoke test..."
    
    local server_path=$(find_bazel_output "//:rbe-server")
    
    if [ -z "$server_path" ] || [ ! -f "$server_path" ]; then
        log_error "Server binary not found"
        exit 1
    fi
    
    # Quick version check
    "$server_path" --version 2>/dev/null || true
    
    log_success "Smoke test passed"
}

# Main execution
main() {
    local target="${1:-all}"
    
    log_info "Building FerrisRBE with Bazel"
    log_info "Project root: $PROJECT_ROOT"
    
    check_bazel_installed
    
    case "$target" in
        server)
            build_server
            ;;
        worker)
            build_worker
            ;;
        image)
            build_image
            ;;
        all)
            build_server
            build_worker
            build_image
            smoke_test
            ;;
        *)
            echo "Usage: $0 [server|worker|image|all]"
            echo ""
            echo "Targets:"
            echo "  server  - Build rbe-server binary"
            echo "  worker  - Build rbe-worker binary"
            echo "  image   - Build and load OCI image"
            echo "  all     - Build everything (default)"
            echo ""
            echo "Note: Handles custom --symlink_prefix automatically"
            exit 1
            ;;
    esac
    
    log_success "Build complete!"
    
    # Print output locations
    log_info "Output locations (symlink_prefix independent):"
    local bazel_bin=$(get_bazel_bin)
    echo "  Bazel bin: $bazel_bin"
    echo "  Use: bazel info bazel-bin to get this path"
}

main "$@"
