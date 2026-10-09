#!/usr/bin/env bash
# A remote-apis-testing client against the local FerrisRBE (//local:up); see README.md.
# Usage: bazel run //conformance:run -- <worker-image|bazel|recc> [bazel args...]
set -euo pipefail

# Versions of matrix.yml at remote-apis-testing 575ce53.
BAZEL_VERSION=9.2.0
BUILDBOX_VERSION=1.4.17
LIBYAML_VERSION=0.2.5
LIBCYAML_VERSION=1.4.2

here="${BUILD_WORKSPACE_DIRECTORY:?run through bazel run}/conformance"
endpoint="host.containers.internal:${FERRIS_LOCAL_GRPC_PORT:-9094}"
worker_image="localhost/ferrisrbe-conformance-worker"
client="${1:?usage: run <worker-image|bazel|recc> [bazel args...]}"
shift

image() {
  podman build --quiet --target "$1" --tag "localhost/ferrisrbe-conformance-$1" \
    --build-arg BAZEL_VERSION="$BAZEL_VERSION" \
    --build-arg BUILDBOX_VERSION="$BUILDBOX_VERSION" \
    "$here" >/dev/null
  echo "localhost/ferrisrbe-conformance-$1"
}

if [[ "$client" != worker-image ]]; then
  running="$(podman inspect --format '{{.ImageName}}' ferrisrbe-local-worker 2>/dev/null || true)"
  if [[ "${running%:latest}" != "$worker_image" ]]; then
    echo "The local worker runs '${running:-nothing}'; the matrix clients expect the toolchain of its build_env:" >&2
    echo "  bazel run //conformance:run -- worker-image" >&2
    echo "  RBE_EXTRA_PLATFORM_PROPERTIES=CHROOT FERRIS_LOCAL_WORKER_LABELS=os=linux,arch=arm64,CHROOT=false FERRIS_LOCAL_WORKER_IMAGE=$worker_image bazel run //local:up" >&2
    exit 2
  fi
fi

case "$client" in
  worker-image)
    podman build --quiet --target build_env --tag "$worker_image" "$here" >/dev/null
    echo "$worker_image"
    ;;
  bazel)
    case "$(podman info --format '{{.Host.Arch}}')" in
      arm64) arch=aarch64 ;;
      amd64) arch=x86_64 ;;
      *) echo "unsupported podman arch" >&2; exit 2 ;;
    esac
    exec podman run --rm \
      -v "$here/clients/bazel:/work:ro,z" -w /work \
      "$(image bazel)" bazel build \
      --lockfile_mode=off \
      --repo_contents_cache= \
      --action_env=BAZEL_DO_NOT_DETECT_CPP_TOOLCHAIN=1 \
      --extra_toolchains="@gcc_toolchain_$arch//:cc_toolchain" \
      --host_platform="//:linux_$arch" \
      --platforms="//:linux_$arch" \
      --extra_execution_platforms="//:linux_$arch" \
      --linkopt=-lstdc++ \
      --linkopt=-lm \
      --remote_instance_name=remote-execution \
      --remote_default_exec_properties=OSFamily=linux \
      --remote_default_exec_properties=CHROOT=false \
      --remote_executor="grpc://$endpoint" \
      --noremote_accept_cached \
      --symlink_prefix=/ \
      "$@" @abseil-hello//:hello_main
    ;;
  recc)
    exec podman run --rm \
      -e RECC_SERVER="$endpoint" \
      -e RECC_INSTANCE=remote-execution \
      -e RECC_REMOTE_PLATFORM_OSFamily=linux \
      -e RECC_REMOTE_PLATFORM_CHROOT=false \
      -e RECC_RETRY_LIMIT=5 \
      -e RECC_RETRY_DELAY=1000 \
      -e RECC_SKIP_CACHE=1 \
      -e CC="/usr/local/bin/recc /usr/bin/gcc" \
      -e LIBYAML_VERSION="$LIBYAML_VERSION" \
      -e LIBCYAML_VERSION="$LIBCYAML_VERSION" \
      "$(image recc)" sh -euxc '
        prefix=/tmp/local
        cd /tmp
        curl -fsSL -o libyaml.tar.gz "https://github.com/yaml/libyaml/releases/download/$LIBYAML_VERSION/yaml-$LIBYAML_VERSION.tar.gz"
        tar -xzf libyaml.tar.gz
        cd "yaml-$LIBYAML_VERSION"
        ./configure --prefix="$prefix" --disable-shared --enable-static
        make -j"$(nproc)"
        make install
        cd ..
        git clone --depth=1 --branch "v$LIBCYAML_VERSION" https://github.com/tlsa/libcyaml.git
        cd libcyaml
        cp "$prefix/include/yaml.h" include/yaml.h
        export PKG_CONFIG_PATH="$prefix/lib/pkgconfig"
        make
        make test
      '
    ;;
  *)
    echo "unknown client: $client" >&2
    exit 2
    ;;
esac
