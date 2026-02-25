# Testing Examples for FerrisRBE

This directory contains the `enterprise` test suite designed to thoroughly stress-test the local FerrisRBE cluster under severe loads, validating its suitability for massive monorepos.

## Structure

```
examples/
├── enterprise/      # Enterprise stress-tests suite
└── README.md        # This file
```

## Prerequisites

### 1. Kubernetes Cluster on Docker Desktop

Make sure you have the cluster running:

```bash
# Deploy the full stack
cd /Users/abel/code/ferrisrbe
./build-and-deploy.sh

# Verify that the pods are running
kubectl get pods -n rbe
```

### 2. Port Forwarding

Start the port-forwards in separate terminals:

```bash
# Terminal 1: RBE Server (execution)
kubectl port-forward -n rbe svc/rbe-server 9092:9092

# Terminal 2: Bazel Remote (cache)
kubectl port-forward -n rbe svc/bazel-remote 9094:9094
```

## Running the Enterprise Stress Tests

The `enterprise` project is configured to stress test four critical vectors of the remote build execution protocol:

1. **Large CAS Payloads**: Testing `ByteStream` performance moving 100MB+ blobs.
2. **Massive Directory Trees**: Passing inputs with tens of thousands of files to stress `GetTree`.
3. **Heavy Concurrency**: Compiling `abseil-cpp` to spawn hundreds of concurrent worker actions.
4. **Resilience/Timeouts**: Injecting intentional flakiness to verify proper resource reclamation.

To launch the ultimate stress test against your local cluster, run:

```bash
cd examples/enterprise

# Wipe the local cache completely clean
bazel clean --expunge

# Execute the massive workload
# The `.bazelrc` will inject --jobs=200 and route traffic to the RBE cluster
bazel test //... --config=k8s
# or
bazel build //... --config=k8s
```

### Expected Behavior

- You should see massive spikes in memory and CPU inside your Docker Desktop VM.
- Bazel should report uploading/downloading large files in the CAS.
- The compilations should route to the remote cluster successfully.
- Note: If `timeout_stress` times out successfully, Bazel will report a failure for that specific target, which is the expected result indicating FerrisRBE successfully enforced the timeout policy.
