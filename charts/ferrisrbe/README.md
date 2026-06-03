# FerrisRBE Helm Chart

[FerrisRBE](https://github.com/xangcastle/ferrisrbe) is a high-performance Remote Build Execution (RBE) server for Bazel, implemented in Rust.

## Prerequisites

- Kubernetes 1.24+
- Helm 3.12+
- Container runtime with image pull access

## Installation

### Add the Helm repository

```bash
helm repo add ferrisrbe https://xangcastle.github.io/ferrisrbe/charts
helm repo update
```

### Install the chart

```bash
# Create namespace
kubectl create namespace rbe

# Install with default values
helm install ferrisrbe ferrisrbe/ferrisrbe -n rbe

# Or with custom values
helm install ferrisrbe ferrisrbe/ferrisrbe -n rbe -f values-custom.yaml
```

## Configuration

### NodePort (Recommended for local development)

```yaml
server:
  service:
    type: NodePort
    nodePort: 30092
```

### LoadBalancer (Cloud environments)

```yaml
server:
  service:
    type: LoadBalancer
```

### Autoscaling Configuration

```yaml
worker:
  autoscaling:
    enabled: true
    minReplicas: 2
    maxReplicas: 20
    targetCPUUtilizationPercentage: 70
```

### Resource Limits

```yaml
server:
  resources:
    limits:
      cpu: 2000m
      memory: 4Gi
    requests:
      cpu: 1000m
      memory: 2Gi

worker:
  resources:
    limits:
      cpu: 4000m
      memory: 8Gi
    requests:
      cpu: 2000m
      memory: 4Gi
```

## All Configuration Options

| Parameter | Description | Default |
|-----------|-------------|---------|
| `server.enabled` | Enable server deployment | `true` |
| `server.replicaCount` | Number of server replicas | `1` |
| `server.image.repository` | Server image repository | `xangcastle/ferris-server` |
| `server.image.tag` | Server image tag | `latest` |
| `server.service.type` | Service type | `NodePort` |
| `server.service.nodePort` | NodePort for gRPC | `30092` |
| `worker.enabled` | Enable worker deployment | `true` |
| `worker.replicaCount` | Number of worker replicas | `2` |
| `worker.image.repository` | Worker image repository | `xangcastle/ferris-worker` |
| `worker.image.tag` | Worker image tag | `latest` |
| `worker.autoscaling.enabled` | Enable HPA | `true` |
| `worker.autoscaling.minReplicas` | Minimum worker replicas | `2` |
| `worker.autoscaling.maxReplicas` | Maximum worker replicas | `10` |
| `config.maxGrpcMsgSize` | Max gRPC message size | `104857600` (100MB) |
| `config.l1CacheCapacity` | L1 cache capacity | `100000` |
| `config.downloadTimeoutSecs` | Download timeout | `300` |
| `config.streamingThreshold` | Streaming threshold | `4194304` (4MB) |

## Bazel Configuration

### Remote Cache Only (Mac/Linux clients)

```bash
# .bazelrc
build:remote-cache --remote_cache=grpc://localhost:30092
build:remote-cache --remote_upload_local_results=true
build:remote-cache --remote_local_fallback
```

### Remote Execution (Linux clients only)

```bash
# .bazelrc
build:remote-exec --remote_executor=grpc://localhost:30092
build:remote-exec --remote_cache=grpc://localhost:30092
build:remote-exec --remote_default_exec_properties=OSFamily=linux
build:remote-exec --remote_default_exec_properties=container-image=
```

## Verification

```bash
# Check pods
kubectl get pods -n rbe

# Check logs
kubectl logs -n rbe -l app.kubernetes.io/component=server
kubectl logs -n rbe -l app.kubernetes.io/component=worker

# Test connectivity
grpcurl -plaintext localhost:30092 build.bazel.remote.execution.v2.Capabilities/GetCapabilities
```

## Uninstallation

```bash
helm uninstall ferrisrbe -n rbe
kubectl delete namespace rbe
```

## License

[MIT](https://github.com/xangcastle/ferrisrbe/blob/main/LICENSE)
