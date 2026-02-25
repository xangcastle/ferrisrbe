#!/bin/bash
#
# FerrisRBE Helm Installation Script
# Usage: ./install.sh [namespace] [nodePort]
#

set -e

NAMESPACE="${1:-rbe}"
NODE_PORT="${2:-30092}"
CHART_URL="https://xangcastle.github.io/ferrisrbe/charts"

echo "=========================================="
echo "FerrisRBE Helm Installer"
echo "=========================================="
echo ""
echo "Namespace: $NAMESPACE"
echo "NodePort: $NODE_PORT"
echo ""

# Check prerequisites
command -v kubectl >/dev/null 2>&1 || { echo "Error: kubectl is required but not installed."; exit 1; }
command -v helm >/dev/null 2>&1 || { echo "Error: helm is required but not installed."; exit 1; }

# Create namespace
echo "Creating namespace..."
kubectl create namespace "$NAMESPACE" 2>/dev/null || echo "Namespace already exists"

# Add Helm repo
echo "Adding FerrisRBE Helm repository..."
helm repo add ferrisrbe "$CHART_URL" 2>/dev/null || helm repo update

# Install chart
echo "Installing FerrisRBE..."
helm install ferrisrbe ferrisrbe/ferrisrbe \
  --namespace "$NAMESPACE" \
  --set server.service.type=NodePort \
  --set server.service.nodePort=$NODE_PORT \
  --set bazelRemote.service.nodePortGrpc=30094 \
  --set bazelRemote.service.nodePortHttp=30080 \
  --wait

echo ""
echo "=========================================="
echo "Installation Complete!"
echo "=========================================="
echo ""
echo "Services:"
echo "  RBE Server (gRPC): localhost:$NODE_PORT"
echo "  Bazel-Remote gRPC: localhost:30094"
echo "  Bazel-Remote HTTP: localhost:30080"
echo ""
echo "Check status:"
echo "  kubectl get pods -n $NAMESPACE"
echo ""
echo "View logs:"
echo "  kubectl logs -n $NAMESPACE -l app.kubernetes.io/component=server"
echo ""
echo "Configure Bazel (.bazelrc):"
echo "  build:remote-cache --remote_cache=grpc://localhost:$NODE_PORT"
echo "  build:remote-cache --remote_upload_local_results=true"
echo ""
