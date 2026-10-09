#!/usr/bin/env bash
# A local FerrisRBE: the server on this machine and a Linux worker in podman.
# Usage: bazel run //local:up   (Ctrl-C stops both)
set -euo pipefail

server_bin="$(realpath "$1")"
worker_bin="$(realpath "$2")"

state="${FERRIS_LOCAL_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/ferrisrbe-local}"
grpc_port="${FERRIS_LOCAL_GRPC_PORT:-9094}"
http_port="${FERRIS_LOCAL_HTTP_PORT:-8088}"
slots="${FERRIS_LOCAL_SLOTS:-16}"
container="ferrisrbe-local-worker"
volume="ferrisrbe-local-workspace"

mkdir -p "$state/server" "$state/bin"
# podman mounts real files only; the runfiles entry is a symlink into the output base.
cp -f "$worker_bin" "$state/bin/worker"

stop() {
  podman rm -f "$container" >/dev/null 2>&1 || true
  [[ -n "${server_pid:-}" ]] && kill "$server_pid" 2>/dev/null || true
}
trap stop EXIT

env RUST_LOG="${RUST_LOG:-info,rbe_server::server::byte_stream=warn}" \
  RBE_BIND_ADDRESS=0.0.0.0 RBE_PORT="$grpc_port" RBE_HTTP_PORT="$http_port" \
  RBE_CACHE_DIR="$state/server" RBE_BES_DATA_DIR="$state/server/bes" \
  "$server_bin" >"$state/server.log" 2>&1 &
server_pid=$!

for _ in $(seq 1 120); do
  curl -fs "http://localhost:$http_port/readyz" >/dev/null 2>&1 && break
  kill -0 "$server_pid" 2>/dev/null || { tail -20 "$state/server.log"; exit 1; }
  sleep 0.5
done
echo "server: grpc://localhost:$grpc_port, http://localhost:$http_port (log: $state/server.log)"

# Every run starts with an empty blob cache, like a freshly scheduled worker.
podman rm -f "$container" >/dev/null 2>&1 || true
podman volume rm -f "$volume" >/dev/null 2>&1 || true
podman volume create "$volume" >/dev/null

echo "worker: $slots slots in podman ($container); build with --config=rbe-local from the monorepo root"
podman run --name "$container" --rm \
  -v "$state/bin:/app:ro,z" -v "$volume:/workspace" \
  -e RUST_LOG="${RUST_LOG:-info}" -e WORKER_ID=local \
  -e SERVER_ENDPOINT="http://host.containers.internal:$grpc_port" \
  -e CAS_ENDPOINT="http://host.containers.internal:$grpc_port" \
  -e MAX_CONCURRENT="$slots" -e RBE_MAX_CONCURRENT_DOWNLOADS=32 \
  -e WORKDIR=/workspace -e WORKER_LABELS="${FERRIS_LOCAL_WORKER_LABELS:-os=linux,arch=arm64}" \
  -e RBE_WORKER_CACHE_MAX_GB="${FERRIS_LOCAL_CACHE_GB:-40}" \
  -e RBE_WORKER_NETWORK="${FERRIS_LOCAL_NETWORK:-on}" \
  "${FERRIS_LOCAL_WORKER_IMAGE:-docker.io/library/debian:bookworm-slim}" /app/worker
