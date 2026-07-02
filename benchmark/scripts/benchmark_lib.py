#!/usr/bin/env python3
"""Shared utilities for the FerrisRBE Python benchmark suite.

These helpers are used by the individual benchmark scripts so that common
behaviour (building a simple REAPI action, computing latency percentiles,
configuring gRPC channels, etc.) lives in one place.
"""

import hashlib
import json
import statistics
from typing import Any, Dict, List, Optional

import grpc

from build.bazel.remote.execution.v2 import remote_execution_pb2


def percentile(data: List[float], p: float) -> float:
    """Return the p-th percentile of a list of numbers."""
    if not data:
        return 0.0
    sorted_data = sorted(data)
    index = int(len(sorted_data) * p / 100)
    return sorted_data[min(index, len(sorted_data) - 1)]


def make_channel(
    server: str,
    options: Optional[List[tuple]] = None,
) -> grpc.Channel:
    """Create an insecure gRPC channel with sensible benchmark defaults."""
    if options is None:
        options = [
            ("grpc.keepalive_time_ms", 30000),
            ("grpc.keepalive_timeout_ms", 10000),
            ("grpc.http2.max_pings_without_data", 0),
            ("grpc.http2.min_time_between_pings_ms", 30000),
        ]
    return grpc.insecure_channel(server, options=options)


def create_simple_action(
    command: List[str],
    output_paths: Optional[List[str]] = None,
) -> tuple:
    """Create a minimal REAPI Action with its dependencies.

    Returns a tuple with:
      (action_digest, action_bytes,
       command_digest, command_bytes,
       input_root_digest, input_root_bytes)
    """
    if output_paths is None:
        output_paths = ["output.txt"]

    command_proto = remote_execution_pb2.Command(
        arguments=command,
        output_paths=output_paths,
    )
    command_bytes = command_proto.SerializeToString()
    command_digest = remote_execution_pb2.Digest(
        hash=hashlib.sha256(command_bytes).hexdigest(),
        size_bytes=len(command_bytes),
    )

    # REAPI requires an input_root_digest even for actions without inputs.
    input_root_proto = remote_execution_pb2.Directory()
    input_root_bytes = input_root_proto.SerializeToString()
    input_root_digest = remote_execution_pb2.Digest(
        hash=hashlib.sha256(input_root_bytes).hexdigest(),
        size_bytes=len(input_root_bytes),
    )

    action_proto = remote_execution_pb2.Action(
        command_digest=command_digest,
        input_root_digest=input_root_digest,
        do_not_cache=False,
    )
    action_bytes = action_proto.SerializeToString()
    action_digest = remote_execution_pb2.Digest(
        hash=hashlib.sha256(action_bytes).hexdigest(),
        size_bytes=len(action_bytes),
    )

    return (
        action_digest,
        action_bytes,
        command_digest,
        command_bytes,
        input_root_digest,
        input_root_bytes,
    )


def save_json(path: str, data: Dict[str, Any]) -> None:
    """Write a JSON object to disk in a deterministic, readable format."""
    with open(path, "w") as f:
        json.dump(data, f, indent=2)
        f.write("\n")
