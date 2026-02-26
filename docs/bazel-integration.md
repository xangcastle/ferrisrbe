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

## Hermeticity Best Practices

Maintaining strict hermeticity is absolute when using FerrisRBE. If your environment leaks state, the remote cache is effectively useless.

### Execution Log Parser
To diagnose differing action hashes between local and remote execution, use Bazel's native Execution Log Parser:

```bash
bazel build --config=remote-exec //... --execution_log_compact_file=/tmp/exec.log
```
Compare this log against a local run to pinpoint exact differences in injected variables or inputs.

### Strict Environment Variables
Prevent your host environment from poisoning the remote cache by strictly isolating the `PATH` and other variables in your `.bazelrc`:

```bash
build --action_env=PATH=/bin:/usr/bin:/usr/local/bin
build --incompatible_strict_action_env=true
```

### Non-Hermetic Targets
If an action inherently requires timestamps, absolute paths, or non-deterministic outputs by design, you must explicitly exclude it from the remote cache to avoid poisoning it:

```starlark
genrule(
    name = "generate_timestamp",
    # ...
    tags = ["no-remote-cache"],
)
```

## Persistent Workers Configuration

FerrisRBE fully supports Bazel's formal persistent worker protocol for long-term JIT compilation and AST retention. To leverage this in your custom Starlark rules, specify the correct `execution_requirements`:

```starlark
my_rule = rule(
    implementation = _my_rule_impl,
    attrs = {...},
    execution_requirements = {
        "supports-workers": "1",
        "requires-worker-protocol": "proto", # or "json"
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
