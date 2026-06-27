# Deployment Guide

## Requirements

- **Kubernetes** 1.28+
- **kubectl** configured with cluster access
- **Helm** 3.12+ (optional, for Helm deployment)

## Quick Start

### Helm (Recommended)

```bash
# Add Helm repository
helm repo add ferrisrbe https://xangcastle.github.io/ferrisrbe/charts
helm repo update

# Install with default settings
helm install ferrisrbe ferrisrbe/ferrisrbe \
  --namespace rbe \
  --create-namespace

# With NodePort for local access
helm install ferrisrbe ferrisrbe/ferrisrbe \
  --namespace rbe \
  --set server.service.type=NodePort \
  --set server.service.nodePort=30092 \
  --set bazelRemote.service.nodePortGrpc=30094
```

### kubectl

```bash
# Deploy all components
./k8s/deploy.sh

# Verify deployment
kubectl get pods -n rbe

# Port-forward for local access
./k8s/port-forward.sh
```

## Local Development with Kind

```bash
# 1. Create Kind cluster
kind create cluster --name ferrisrbe

# 2. Build images locally
bazel run //oci:server_load //oci:worker_load

# 3. Load images into Kind
kind load docker-image ferrisrbe/server:latest \
  ferrisrbe/worker:latest --name ferrisrbe

# 4. Deploy with local images
helm install ferrisrbe ./charts/ferrisrbe \
  --namespace rbe \
  --set server.image.repository=ferrisrbe/server \
  --set server.image.pullPolicy=Never \
  --set worker.image.repository=ferrisrbe/worker \
  --set worker.image.pullPolicy=Never
```

## Production Deployment

### Resource Requirements

| Component | CPU Request | CPU Limit | Memory Request | Memory Limit |
|-----------|-------------|-----------|----------------|--------------|
| Server | 100m | 500m | 256Mi | 1Gi |
| Worker | 500m | 2000m | 1Gi | 4Gi |
| rbe-cache | 500m | 2000m | 2Gi | 8Gi |

### High Availability

```bash
# Scale server replicas
helm upgrade ferrisrbe ferrisrbe/ferrisrbe \
  --namespace rbe \
  --set server.replicaCount=3

# Enable worker autoscaling
helm upgrade ferrisrbe ferrisrbe/ferrisrbe \
  --namespace rbe \
  --set worker.autoscaling.enabled=true \
  --set worker.autoscaling.minReplicas=5 \
  --set worker.autoscaling.maxReplicas=100
```

### TLS Configuration

For production, configure TLS for gRPC:

```yaml
# values.yaml override
server:
  tls:
    enabled: true
    certSecret: rbe-tls-cert
```

## Podman Compose (Local Testing)

Local stack with workers, cache, and execution. Images are built with Bazel and loaded into Podman.

```bash
# Build and load images
bazel run //oci:server_load
bazel run //oci:worker_load
bazel run //oci:cache_load

# Start the stack
podman-compose -f podman-compose.yml up -d

# View logs
podman-compose -f podman-compose.yml logs -f

# Stop
podman-compose -f podman-compose.yml down
```

The compose file references locally built images:

- `ferrisrbe/server:latest`
- `ferrisrbe/worker:latest`
- `ferrisrbe/cache:latest`

Exposed ports:

- `localhost:9092` - RBE Server gRPC
- `localhost:9094` - CAS gRPC
- `localhost:8080` - CAS HTTP

## Verification

```bash
# Check pods
kubectl get pods -n rbe

# Check logs
kubectl logs -n rbe -l app.kubernetes.io/component=server --tail=50
kubectl logs -n rbe -l app.kubernetes.io/component=worker --tail=50

# Test connectivity
grpcurl -plaintext localhost:9092 \
  build.bazel.remote.execution.v2.Capabilities/GetCapabilities
```
