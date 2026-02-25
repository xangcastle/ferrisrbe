#!/bin/bash
# Post-start script - runs every time the container starts

set -e

echo "🚀 FerrisRBE environment starting..."

# Check if Docker is available
if docker ps &>/dev/null; then
    echo "✅ Docker is available"
else
    echo "⚠️  Docker not available - some features may not work"
fi

# Optional: Auto-start services
# Uncomment if you want services to start automatically
# echo "Starting services with Docker Compose..."
# docker-compose up -d

# Or create Kind cluster
# if ! kind get clusters | grep -q ferrisrbe; then
#     echo "Creating Kind cluster..."
#     kind create cluster --name ferrisrbe
# fi

echo ""
echo "Ready! Run 'docker-compose up -d' to start the stack."
