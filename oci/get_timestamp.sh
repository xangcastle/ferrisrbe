#!/usr/bin/env bash
# STABLE_ so the stamped genrules rerun: Bazel never re-executes an action
# just because volatile-status.txt changed, which froze `created` in the images.
echo "STABLE_BUILD_TIMESTAMP $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "STABLE_GIT_COMMIT $(git rev-parse --short=12 HEAD 2>/dev/null || echo unknown)"
