# Local FerrisRBE

The server on this machine and a Linux worker in podman, to reproduce a
worker bug and check its fix before anything is deployed.

```bash
bazel run //local:up          # Ctrl-C stops both
```

Then build what failed from the workspace that failed, with every action on
that worker: `--remote_executor=grpc://localhost:9094` and an execution
platform for `linux/arm64`, a separate output base, and
`--noremote_accept_cached` so nothing comes from the cache:

```bash
bazel --output_base=$HOME/.cache/ferrisrbe-local/output_base \
  build --config=rbe-local --noremote_accept_cached --keep_going //your:target
```

- The server listens on `0.0.0.0:9094` (gRPC) and `:8088` (HTTP: `/readyz`,
  `/metrics`, UI), without authentication; its log and data live in
  `~/.cache/ferrisrbe-local` (`FERRIS_LOCAL_DIR`).
- The worker is the `linux/arm64` binary on `debian:bookworm-slim`, the base
  of the production image, with 16 slots (`FERRIS_LOCAL_SLOTS`) and
  `/workspace` on a podman volume recreated on every run: an empty blob cache,
  like a freshly scheduled worker. In `podman machine` that volume is XFS with
  reflink, like the instance-store NVMe of the worker nodes.
  `FERRIS_LOCAL_WORKER_IMAGE` swaps the base image and
  `FERRIS_LOCAL_WORKER_LABELS` its labels (`os=linux,arch=arm64`), as
  [`conformance/`](../conformance/README.md) does.
- The worker gives actions network unless `FERRIS_LOCAL_NETWORK=off`, the
  `RBE_WORKER_NETWORK` of production; a `Network` exec property (`on`/`off`)
  overrides it per action.
- `--output_base` keeps the build away from your usual one, and its local
  action cache: run `bazel clean` against it between two runs you want to
  compare, or Bazel skips the actions that already succeeded.
- `--config=rbe-local` names those flags in the workspace's `.bazelrc`. It
  has to send every action to the worker, the ones a regular remote config
  keeps local (`no-remote-exec`) included, or Linux tools run on the host.

To compare a fix, build `//:worker` for `//platforms:linux_arm64` with and
without it and run the same build against each.

## UI with hot reload

With `//local:up` running, the vite dev server serves `ui/src` live on
`http://localhost:5173` and proxies `/api` (and its websocket) to the local
server; `RBE_API_URL` points it elsewhere:

```bash
RBE_KEYCLOAK_URL=http://localhost:8080/ bazel run //ui:dev
```

`RBE_KEYCLOAK_URL`, `RBE_KEYCLOAK_REALM` and `RBE_KEYCLOAK_CLIENT_ID` fill the
same env-config the server injects in production; the Keycloak client has to
accept `http://localhost:5173/*` as a redirect URI.
