#!/usr/bin/env python3
"""Generate Python protobuf and gRPC stubs using grpcio-tools.

This script is invoked from benchmark/proto/BUILD.bazel and writes generated
_pb2.py and _pb2_grpc.py files into a single output tree that mirrors the
original proto package structure.
"""

import os
import sys

import grpc_tools
from grpc_tools import protoc


def find_proto_include_dir(proto_files):
    """Return the directory that acts as the proto import root.

    The paths passed by Bazel are sandbox paths such as:
        proto/build/bazel/.../remote_execution.proto
    We walk up each path until we find the 'proto' component and return the
    path up to and including 'proto'.  That directory is the include root:
    proto imports like "build/bazel/semver/semver.proto" are resolved inside
    it.
    """
    for pf in proto_files:
        parts = os.path.normpath(pf).split(os.sep)
        if "proto" in parts:
            idx = parts.index("proto")
            return os.sep.join(parts[: idx + 1])
    raise RuntimeError(
        f"Could not locate the 'proto' include root in any of: {proto_files}"
    )


def well_known_types_include():
    """Return the path grpcio-tools uses for protobuf well-known types."""
    return os.path.join(grpc_tools.__path__[0], "_proto")


def main():
    if len(sys.argv) < 3:
        print(
            "Usage: generate_protos.py <output_dir> <proto_file>...",
            file=sys.stderr,
        )
        sys.exit(1)

    out_dir = sys.argv[1]
    proto_files = sys.argv[2:]

    os.makedirs(out_dir, exist_ok=True)
    include_dir = find_proto_include_dir(proto_files)

    # protoc expects paths relative to the include directory.
    rel_proto_files = [os.path.relpath(pf, include_dir) for pf in proto_files]

    args = [
        "protoc",
        f"-I{include_dir}",
        f"-I{well_known_types_include()}",
        f"--python_out={out_dir}",
        f"--grpc_python_out={out_dir}",
    ] + rel_proto_files

    return_code = protoc.main(args)
    if return_code != 0:
        sys.exit(return_code)


if __name__ == "__main__":
    main()
