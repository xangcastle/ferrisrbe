# Monitoring

## Metrics

FerrisRBE exposes metrics in Prometheus format (when enabled).

### Server Metrics

| Metric | Type | Description |
|--------|------|-------------|
| `rbe_executions_total` | Counter | Total executions |
| `rbe_executions_active` | Gauge | Currently active executions |
| `rbe_cache_hits_total` | Counter | L1 cache hits |
| `rbe_cache_misses_total` | Counter | L1 cache misses |
| `rbe_workers_connected` | Gauge | Connected workers |
| `rbe_workers_available` | Gauge | Available (idle) workers |

### Worker Metrics

| Metric | Type | Description |
|--------|------|-------------|
| `rbe_worker_executions_total` | Counter | Executions by this worker |
| `rbe_worker_reconnections_total` | Counter | Reconnection count |
| `rbe_worker_keepalive_interval` | Gauge | Current keepalive interval |

## Logs

Structured JSON logging with configurable levels.

### Log Levels

- `error` - Errors only
- `warn` - Warnings and errors
- `info` - General information (default)
- `debug` - Detailed debugging
- `trace` - Verbose tracing

### Common Log Patterns

**Successful execution:**
```
INFO Execution::Execute digest=abc123...
INFO Enqueued operation op-1
INFO Assigned operation op-1 to worker worker-1
INFO Execution result: exit_code=0
```

**Cache hit:**
```
INFO Cache hit for digest=abc123...
```

**Worker reconnection:**
```
WARN Connection lost, reconnecting...
INFO Connection established successfully
```

## Health Checks

### Kubernetes Probes

The server exposes TCP health checks on the gRPC port:

```yaml
livenessProbe:
  tcpSocket:
    port: 9092
  initialDelaySeconds: 10
  periodSeconds: 30

readinessProbe:
  tcpSocket:
    port: 9092
  initialDelaySeconds: 5
  periodSeconds: 10
```

### Manual Health Check

```bash
# Check server health
nc -z <server-host> 9092

# Check with gRPC
ggrpcurl -plaintext <server-host>:9092 \
  build.bazel.remote.execution.v2.Capabilities/GetCapabilities
```
