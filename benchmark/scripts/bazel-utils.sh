#!/bin/bash
# Bazel utility functions for benchmark scripts
# Handles custom symlink prefixes and output locations

# Get the actual Bazel output directory (handles custom --symlink_prefix)
get_bazel_bin() {
    local workspace="${1:-.}"
    cd "$workspace" && bazel info bazel-bin 2>/dev/null
}

# Get the actual Bazel genfiles directory
get_bazel_genfiles() {
    local workspace="${1:-.}"
    cd "$workspace" && bazel info bazel-genfiles 2>/dev/null
}

# Get Bazel execution root
get_bazel_execution_root() {
    local workspace="${1:-.}"
    cd "$workspace" && bazel info execution_root 2>/dev/null
}

# Find a Bazel output file, searching in multiple possible locations
find_bazel_output() {
    local target="$1"  # e.g., "//:rbe-server"
    local workspace="${2:-.}"
    local output_name="${3:-}"  # Optional: specific output filename
    
    cd "$workspace"
    
    # Get actual bazel-bin path
    local bazel_bin=$(bazel info bazel-bin 2>/dev/null)
    
    if [ -z "$bazel_bin" ]; then
        echo ""
        return 1
    fi
    
    # Convert target to path
    # //:rbe-server -> bazel-bin/rbe-server
    # //src:server -> bazel-bin/src/server
    local target_path=$(echo "$target" | sed 's|//||' | sed 's|^:||' | sed 's|:|/|g')
    
    if [ -n "$output_name" ]; then
        target_path="$target_path/$output_name"
    fi
    
    local full_path="$bazel_bin/$target_path"
    
    if [ -f "$full_path" ]; then
        echo "$full_path"
        return 0
    fi
    
    # Try common variations
    local variations=(
        "$bazel_bin/rbe-server"
        "$bazel_bin/rbe-worker"  
        "$bazel_bin/$output_name"
    )
    
    for path in "${variations[@]}"; do
        if [ -f "$path" ]; then
            echo "$path"
            return 0
        fi
    done
    
    echo ""
    return 1
}

# Build a target and return the output path
bazel_build_and_get_output() {
    local target="$1"
    local workspace="${2:-.}"
    local config="${3:-release}"
    
    cd "$workspace"
    
    # Build
    bazel build "$target" --config="$config" 2>/dev/null || bazel build "$target"
    
    # Find output
    local output=$(find_bazel_output "$target" "$workspace")
    
    if [ -n "$output" ] && [ -f "$output" ]; then
        echo "$output"
        return 0
    fi
    
    echo ""
    return 1
}

# Check if Bazel is available
check_bazel() {
    if ! command -v bazel &> /dev/null; then
        echo "ERROR: Bazel not found" >&2
        return 1
    fi
    
    local version=$(bazel --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+')
    if [ -n "$version" ]; then
        echo "Bazel $version"
        return 0
    fi
    
    return 0
}

# Get workspace root
get_workspace_root() {
    local dir="${1:-.}"
    cd "$dir" && bazel info workspace 2>/dev/null || pwd
}

# Export functions if sourced
if [[ "${BASH_SOURCE[0]}" != "${0}" ]]; then
    export -f get_bazel_bin
    export -f get_bazel_genfiles
    export -f get_bazel_execution_root
    export -f find_bazel_output
    export -f bazel_build_and_get_output
    export -f check_bazel
    export -f get_workspace_root
fi
