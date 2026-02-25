# Configuration

FerrisRBE follows [12-Factor App](https://12factor.net/) methodology. All configuration is via environment variables.

## Server Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `RBE_PORT` | `9092` | Server gRPC port |
| `RBE_BIND_ADDRESS` | `0.0.0.0` | Bind address |
| `RUST_LOG` | `info` | Log level (trace/debug/info/warn/error) |
| `CAS_ENDPOINT` | `bazel-remote:9094` | CAS (bazel-remote) endpoint |
| `REDIS_ENDPOINT` | `redis:6379` | Redis endpoint for metadata |
| `RBE_L1_CACHE_CAPACITY` | `100000` | L1 action cache entry limit |
| `RBE_L1_CACHE_TTL_SECS` | `3600` | L1 cache TTL in seconds |
| `RBE_MAX_BATCH_SIZE` | `4194304` | Max batch size for CAS operations (4MB) |
| `RBE_INLINE_OUTPUT_THRESHOLD` | `1048576` | Inline output threshold (1MB) |

### HTTP/2 Keepalive Settings

| Variable | Default | Description |
|----------|---------|-------------|
| `RBE_TCP_KEEPALIVE_SECS` | `30` | TCP keepalive probe interval |
| `RBE_HTTP2_KEEPALIVE_INTERVAL_SECS` | `20` | HTTP/2 PING frame interval |
| `RBE_HTTP2_KEEPALIVE_TIMEOUT_SECS` | `15` | HTTP/2 PING ACK timeout |
| `RBE_REQUEST_TIMEOUT_SECS` | `600` | Request timeout (0 = disabled) |
| `RBE_HTTP2_ADAPTIVE_WINDOW` | `true` | Enable adaptive flow control |

**Important:** Server HTTP/2 interval/timeout must be >= worker values to prevent connection drops.

## Worker Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `WORKER_ID` | (hostname) | Unique worker identifier |
| `SERVER_ENDPOINT` | `http://rbe-server:9092` | RBE server endpoint |
| `CAS_ENDPOINT` | `http://bazel-remote:9094` | CAS endpoint |
| `WORKER_TYPE` | `default` | Worker type (default, highcpu, gpu) |
| `WORKER_LABELS` | `os=linux,arch=amd64` | Comma-separated labels |
| `MAX_CONCURRENT` | `4` | Maximum concurrent executions |
| `WORKDIR` | `/workspace` | Working directory |
| `RUST_LOG` | `info` | Log level |

### Connection Settings

| Variable | Default | Description |
|----------|---------|-------------|
| `RBE_KEEPALIVE_INTERVAL_SECS` | `20` | HTTP/2 keepalive ping interval |
| `RBE_MIN_KEEPALIVE_SECS` | `10` | Minimum adaptive keepalive interval |
| `RBE_MAX_KEEPALIVE_SECS` | `60` | Maximum adaptive keepalive interval |
| `RBE_KEEPALIVE_TIMEOUT_SECS` | `15` | Keepalive response timeout |
| `RBE_TCP_KEEPALIVE_SECS` | `30` | TCP keepalive interval |
| `RBE_CONNECTION_TIMEOUT_SECS` | `30` | TCP connection timeout |
| `RBE_MAX_RECONNECT_ATTEMPTS` | `10` | Max reconnection attempts |
| `RBE_RECONNECT_BASE_DELAY_MS` | `100` | Base delay for exponential backoff |
| `RBE_RECONNECT_MAX_DELAY_MS` | `30000` | Max delay for exponential backoff |
| `RBE_HEALTH_CHECK_INTERVAL_SECS` | `5` | Health check frequency |
| `RBE_HEALTH_CHECK_TIMEOUT_SECS` | `3` | Health check timeout |

### Materializer Settings

| Variable | Default | Description |
|----------|---------|-------------|
| `RBE_DOWNLOAD_TIMEOUT_SECS` | `300` | Per-file download timeout |
| `RBE_MAX_CONCURRENT_DOWNLOADS` | `10` | Max concurrent downloads |
| `RBE_DOWNLOAD_CHUNK_SIZE` | `65536` | Download chunk size (64KB) |
| `RBE_STREAMING_THRESHOLD` | `4194304` | Streaming threshold (4MB) |
| `RBE_USE_HARDLINKS` | `true` | Use hardlinks when possible |
| `RBE_VALIDATE_DIGESTS` | `true` | Validate digests after download |

## Printing All Options

Run with `RBE_PRINT_CONFIG_OPTIONS=1` to dump all available configuration options:

```bash
export RBE_PRINT_CONFIG_OPTIONS=1
./rbe-server
```
