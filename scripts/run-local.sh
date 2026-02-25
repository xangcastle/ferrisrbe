#!/bin/bash
set -e

echo "╔══════════════════════════════════════════════════════════════╗"
echo "║              Running RBE Server Locally                       ║"
echo "╚══════════════════════════════════════════════════════════════╝"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

cd "$PROJECT_ROOT"

if ! command -v cargo &> /dev/null; then
    echo "❌ Rust/Cargo not found. Please install Rust: https://rustup.rs/"
    exit 1
fi

export RBE_PORT="${RBE_PORT:-9092}"
export RBE_BIND_ADDRESS="${RBE_BIND_ADDRESS:-127.0.0.1}"
export RUST_LOG="${RUST_LOG:-info}"

echo ""
echo "Configuration:"
echo "  Port: $RBE_PORT"
echo "  Bind Address: $RBE_BIND_ADDRESS"
echo "  Log Level: $RUST_LOG"
echo ""

cargo run --release
