#!/bin/bash
#
# Release script for FerrisRBE Helm charts
# Usage: ./release.sh [version]
#

set -e

VERSION="${1:-0.1.0}"
CHART_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "=========================================="
echo "FerrisRBE Chart Release"
echo "=========================================="
echo ""
echo "Version: $VERSION"
echo ""

# Update version in Chart.yaml
echo "Updating Chart.yaml..."
sed -i.bak "s/version: .*/version: $VERSION/" "$CHART_DIR/ferrisrbe/Chart.yaml"
rm "$CHART_DIR/ferrisrbe/Chart.yaml.bak"

# Package chart
echo "Packaging chart..."
cd "$CHART_DIR"
helm package ferrisrbe/

# Update index
echo "Updating index..."
helm repo index . --url https://xangcastle.github.io/ferrisrbe/charts

# Remove old packages (keep only latest 3 versions of each chart)
echo "Cleaning old packages..."
for chart in ferrisrbe; do
  ls -t "$chart"-*.tgz 2>/dev/null | tail -n +4 | xargs -r rm -f
done

echo ""
echo "=========================================="
echo "Release Complete!"
echo "=========================================="
echo ""
echo "Next steps:"
echo "1. Commit the changes:"
echo "   git add charts/"
echo "   git commit -m \"Release chart v$VERSION\""
echo "   git push"
echo ""
echo "2. For Docker Hub images, push them:"
echo "   docker push xangcastle/ferris-server:latest"
echo "   docker push xangcastle/ferris-worker:latest"
echo ""
