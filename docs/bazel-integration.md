# Bazel Integration

## Configuration

Add to your `.bazelrc`:

```bash
# ============================================
# Remote Cache (works from any OS)
# ============================================
build:remote-cache --remote_cache=grpc://localhost:9092
build:remote-cache --remote_upload_local_results=true
build:remote-cache --remote_timeout=600

# ============================================
# Remote Execution (requires Linux toolchains)
# ============================================
build:remote-exec --config=remote-cache
build:remote-exec --remote_executor=grpc://localhost:9092

# Platform for remote execution
build:remote-exec --extra_execution_platforms=//toolchains:linux_x86_64
build:remote-exec --platforms=//toolchains:linux_x86_64

# Useful options
build:remote --remote_download_minimal
build:remote --remote_local_fallback
```

## Usage

### Caching Only

```bash
bazel build --config=remote-cache //...
```

### Full Remote Execution

```bash
bazel build --config=remote-exec //...
```

### View Cache Statistics

```bash
bazel build --config=remote-cache //... 2>&1 | grep -E "(remote cache hit|processes)"
```

Example output:
```
INFO: 47 processes: 30 remote cache hit, 10 internal, 7 darwin-sandbox.
```

## Platform Configuration

Define execution platforms in `toolchains/BUILD.bazel`:

```starlark
platform(
    name = "linux_x86_64",
    constraint_values = [
        "@platforms//os:linux",
        "@platforms//cpu:x86_64",
    ],
    exec_properties = {
        "OSFamily": "Linux",
        "container-image": "docker://gcr.io/bazel-public/ubuntu2004-java11",
    },
)

platform(
    name = "linux_arm64",
    constraint_values = [
        "@platforms//os:linux",
        "@platforms//cpu:arm64",
    ],
    exec_properties = {
        "OSFamily": "Linux",
        "container-image": "docker://arm64v8/ubuntu:20.04",
    },
)
```

## Bazel Version Compatibility

FerrisRBE is tested with:

| Bazel Version | Status | Notes |
|---------------|--------|-------|
| 7.4.x | ✅ Supported | Full REAPI v2.4 |
| 8.0.x | ✅ Supported | Full REAPI v2.4 |
| 8.1.x | ✅ Supported | Full REAPI v2.4 |
| 9.x | ✅ Supported | Latest features |

## Troubleshooting

### Cache Misses

If you're seeing unexpected cache misses:

1. Verify action keys are deterministic
2. Check for absolute paths in inputs
3. Ensure timestamps are normalized

```bash
# Debug cache misses
bazel build --config=remote-cache //... \
  --experimental_remote_cache_compression \
  --remote_print_execution_messages=all
```

### Timeout Issues

```bash
# Increase timeouts for large builds
build:remote --remote_timeout=3600
```
