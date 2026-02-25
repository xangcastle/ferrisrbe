---
name: Bug report
about: Create a report to help us improve
title: '[BUG] '
labels: bug
assignees: ''

---

**Describe the bug**
A clear and concise description of what the bug is.

**To Reproduce**
Steps to reproduce the behavior:
1. Deploy FerrisRBE version '...'
2. Configure Bazel with '...'
3. Run command: `bazel build //... --config=remote`
4. See error in worker logs / server logs / Bazel output

**Expected behavior**
A clear and concise description of what you expected to happen.

**Logs**

*Server logs:*
```
kubectl logs -n rbe -l app.kubernetes.io/component=server --tail=50
```

*Worker logs:*
```
kubectl logs -n rbe -l app.kubernetes.io/component=worker --tail=50
```

*Bazel output (with --verbose_failures):*
```
Paste relevant Bazel output here
```

**Environment:**
 - OS (client): [e.g. macOS 14, Ubuntu 22.04]
 - Kubernetes version: [e.g. 1.28]
 - FerrisRBE version: [e.g. 0.1.0 or git commit]
 - Bazel version: [e.g. 7.4.0]
 - Deployment method: [e.g. Helm, kubectl, Docker Compose]

**Configuration**

*Relevant `.bazelrc` settings:*
```
build:remote --remote_executor=grpc://...
build:remote --remote_cache=grpc://...
```

*Helm values (if applicable):*
```yaml
server:
  replicaCount: 1
worker:
  replicaCount: 2
```

**Additional context**
- Are you using Remote Cache, Remote Execution, or both?
- Does the issue happen consistently or intermittently?
- Any network proxies or special infrastructure between Bazel and the cluster?
