# Troubleshooting

## Workers Not Connecting

### Symptoms
- Workers in `CrashLoopBackOff` state
- "No workers available" errors in server logs

### Solutions

```bash
# Check worker logs
kubectl logs -n rbe -l app.kubernetes.io/component=worker --tail=50

# Verify service accessibility
kubectl get svc -n rbe

# Check if workers can reach server
kubectl exec -n rbe deploy/ferrisrbe-worker -- \
  nc -zv ferrisrbe-server 9092
```

## Bazel Cannot Find Server

### Symptoms
- "Connection refused" errors
- "Unavailable" gRPC status

### Solutions

```bash
# Verify connectivity
grpcurl -plaintext localhost:9092 \
  build.bazel.remote.execution.v2.Capabilities/GetCapabilities

# Check server logs
kubectl logs -n rbe -l app.kubernetes.io/component=server --tail=50

# Verify service endpoints
kubectl get endpoints -n rbe
```

## Bazel Hangs on "remote"

### Symptoms
- Build stuck at "Building ..."
- No progress on actions

### Solutions

```bash
# Verify workers are registered
kubectl logs -n rbe -l app.kubernetes.io/component=server | \
  grep "Worker registration"

# Check worker status
kubectl get pods -n rbe -l app.kubernetes.io/component=worker

# Verify scheduler has available workers
kubectl logs -n rbe -l app.kubernetes.io/component=server | \
  grep -E "(No workers available|Selected worker)"
```

## HTTP/2 Connection Errors

### Symptoms
- "h2 protocol error: error reading a body from connection"
- Workers disconnect after ~40s

### Cause
Keepalive timeout mismatch between server and worker.

### Solution
Ensure server HTTP/2 settings >= worker settings:

```yaml
# Server (must be >= worker)
RBE_HTTP2_KEEPALIVE_INTERVAL_SECS: "20"
RBE_HTTP2_KEEPALIVE_TIMEOUT_SECS: "15"

# Worker
RBE_KEEPALIVE_INTERVAL_SECS: "20"
RBE_KEEPALIVE_TIMEOUT_SECS: "15"
```

## Cache Misses

### Symptoms
- No "remote cache hit" in build output
- Slower builds than expected

### Solutions

1. Verify cache is working:
```bash
bazel build --config=remote //... 2>&1 | grep "remote cache hit"
```

2. Check action determinism:
```bash
bazel build --config=remote //... && \
bazel build --config=remote //... 2>&1 | grep "cache hit"
```

3. Verify cache storage:
```bash
kubectl exec -n rbe deploy/ferrisrbe-bazel-remote -- \
  ls -la /data/cas
```

## OOM Killed

### Symptoms
- Pods restarting with `OOMKilled` status
- Container exit code 137

### Solutions

Increase memory limits:

```yaml
# values.yaml
worker:
  resources:
    limits:
      memory: "8Gi"  # Increase from 4Gi
```

Or reduce concurrency:

```yaml
config:
  maxConcurrent: 2  # Reduce from 4
```

## Getting Help

- GitHub Issues: [github.com/xangcastle/ferrisrbe/issues](https://github.com/xangcastle/ferrisrbe/issues)
- Check logs: `kubectl logs -n rbe --all-containers --since=1h`
- Enable debug logging: `RUST_LOG=debug`
