#!/usr/bin/env bash

set -e

OUTPUT_FILE="$1"

TIMESTAMP=""

if [ -f "bazel-out/stable-status.txt" ]; then
    TIMESTAMP=$(grep "^STABLE_BUILD_TIMESTAMP " bazel-out/stable-status.txt 2>/dev/null | cut -d' ' -f2 || echo "")
fi

if [ -n "$TIMESTAMP" ]; then
    case "$TIMESTAMP" in
        *T*)
            ;;
        *)
            TIMESTAMP=$(date -u -d "@$TIMESTAMP" +'%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || \
                       date -u -r "$TIMESTAMP" +'%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || \
                       echo "")
            ;;
    esac
fi

if [ -z "$TIMESTAMP" ]; then
    TIMESTAMP=$(date -u +'%Y-%m-%dT%H:%M:%SZ')
fi

echo -n "$TIMESTAMP" | tr -d '\n\r' > "$OUTPUT_FILE"
