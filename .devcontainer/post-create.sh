#!/bin/bash
# Post-create script for GitHub Codespaces
# Runs after the container is created

set -e

echo "🦀 Setting up FerrisRBE development environment..."

# Install Bazelisk
if ! command -v bazel &> /dev/null; then
    echo "Installing Bazelisk..."
    curl -Lo /tmp/bazel https://github.com/bazelbuild/bazelisk/releases/latest/download/bazelisk-linux-amd64
    chmod +x /tmp/bazel
    sudo mv /tmp/bazel /usr/local/bin/bazel
fi

# Install Kind for local Kubernetes
if ! command -v kind &> /dev/null; then
    echo "Installing Kind..."
    curl -Lo /tmp/kind https://kind.sigs.k8s.io/dl/v0.22.0/kind-linux-amd64
    chmod +x /tmp/kind
    sudo mv /tmp/kind /usr/local/bin/kind
fi

# Install grpcurl for testing
if ! command -v grpcurl &> /dev/null; then
    echo "Installing grpcurl..."
    curl -Lo /tmp/grpcurl.tar.gz https://github.com/fullstorydev/grpcurl/releases/download/v1.8.9/grpcurl_1.8.9_linux_x86_64.tar.gz
    tar -xzf /tmp/grpcurl.tar.gz -C /tmp
    sudo mv /tmp/grpcurl /usr/local/bin/
    rm /tmp/grpcurl.tar.gz
fi

# Install cargo tools
echo "Installing Rust tools..."
cargo install --locked cargo-binstall 2>/dev/null || true
cargo binstall -y cargo-watch cargo-expand 2>/dev/null || true

# Verify installations
echo ""
echo "✅ Verifying installations:"
echo "Rust: $(rustc --version)"
echo "Bazel: $(bazel --version)"
echo "kubectl: $(kubectl version --client --short 2>/dev/null || echo 'installed')"
echo "Helm: $(helm version --short 2>/dev/null || echo 'installed')"

# Initial build (optional, can be slow)
# echo ""
# echo "🔨 Running initial build..."
# bazel build //:rbe-server //:rbe-worker 2>&1 | tail -5

echo ""
echo "╔════════════════════════════════════════════════════════════╗"
echo "║              Setup Complete! 🎉                            ║"
echo "╠════════════════════════════════════════════════════════════╣"
echo "║ Quick Start:                                               ║"
echo "║   bazel build //:rbe-server //:rbe-worker  Build           ║"
echo "║   ./scripts/run-local.sh                    Run locally    ║"
echo "║   docker-compose up -d                        Run with Docker║"
echo "║   kind create cluster --name ferrisrbe      Create K8s     ║"
echo "╚════════════════════════════════════════════════════════════╝"
