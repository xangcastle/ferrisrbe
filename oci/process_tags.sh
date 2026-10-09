#!/usr/bin/env bash

set -e

OUTPUT_FILE="$1"

COMMIT=""
if [ -f "bazel-out/stable-status.txt" ]; then
    COMMIT=$(grep "^STABLE_GIT_COMMIT " bazel-out/stable-status.txt 2>/dev/null | cut -d' ' -f2 || echo "")
fi

# `latest` keeps the manual path working; the commit tag is what ArgoCD
# Image Updater picks up as a new build.
{
    echo "latest"
    if [ -n "$COMMIT" ] && [ "$COMMIT" != "unknown" ]; then
        echo "$COMMIT"
    fi
} > "$OUTPUT_FILE"
