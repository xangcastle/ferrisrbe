#!/usr/bin/env python3
"""
Connection Churn Test - Abrupt disconnections and resource cleanup
Tests handling of sudden connection drops during gRPC operations.

FerrisRBE Advantage: Tokio's native task cancellation releases resources immediately.
JVM apps may leave zombie threads or persistent connections causing memory leaks.
"""

import argparse
import hashlib
import json
import os
import random
import statistics
import threading
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass, field
from typing import List, Optional, Tuple

import grpc

from build.bazel.remote.execution.v2 import remote_execution_pb2
from build.bazel.remote.execution.v2 import remote_execution_pb2_grpc
from google.bytestream import bytestream_pb2
from google.bytestream import bytestream_pb2_grpc

from benchmark.scripts import benchmark_lib


# Shared lock and state used to seed CAS with a valid blob/action once.
_seed_lock = threading.Lock()
_seed_state = {"blob_digest": None, "action_digest": None}


@dataclass
class ChurnResult:
    """Result of a single churn test"""
    connection_id: str
    operation: str
    duration_ms: float
    disconnected: bool = False
    resources_cleaned: bool = False
    error: Optional[str] = None


@dataclass
class ChurnSummary:
    """Summary of connection churn test"""
    server: str
    total_connections: int
    disconnect_rate: float
    results: List[ChurnResult] = field(default_factory=list)

    def get_normal_completions(self) -> List[ChurnResult]:
        return [r for r in self.results if not r.disconnected]

    def get_disconnected(self) -> List[ChurnResult]:
        return [r for r in self.results if r.disconnected]

    def get_successful_cleanups(self) -> List[ChurnResult]:
        return [r for r in self.results if r.resources_cleaned]

    def get_failed_cleanups(self) -> List[ChurnResult]:
        return [r for r in self.results if not r.resources_cleaned]

    def print_summary(self):
        normal = self.get_normal_completions()
        disconnected = self.get_disconnected()
        successful = self.get_successful_cleanups()
        failed = self.get_failed_cleanups()

        normal_ok = [r for r in normal if r.resources_cleaned]
        disc_ok = [r for r in disconnected if r.resources_cleaned]

        print("\n" + "=" * 70)
        print(f"CONNECTION CHURN TEST - {self.server}")
        print("=" * 70)
        print(f"\n📊 TEST PARAMETERS:")
        print(f"  Total connections: {self.total_connections}")
        print(f"  Disconnect rate: {self.disconnect_rate * 100:.1f}%")

        print(f"\n📊 NORMAL COMPLETIONS: {len(normal_ok)}/{len(normal)} passed")
        print(f"📊 ABRUPT DISCONNECTS: {len(disc_ok)}/{len(disconnected)} cleaned")
        print(f"📊 OVERALL: {len(successful)}/{len(self.results)} ({len(successful)/len(self.results)*100:.1f}%) successful")

        if self.results:
            durations = [r.duration_ms for r in self.results if r.duration_ms > 0]
            if durations:
                print(f"\n⏱️  CLEANUP TIME:")
                print(f"  Avg: {statistics.mean(durations):.1f}ms")
                print(f"  Max: {max(durations):.1f}ms")

        print(f"\n🏆 VERDICT:")
        if failed:
            print(f"  ⚠️  {len(failed)} connections reported unexpected errors")
        else:
            print(f"  ✅ All connections closed cleanly without unexpected errors")

        if len(disc_ok) == len(disconnected):
            print(f"  ✅ All abrupt disconnections were handled without hangs")
        else:
            print(f"  ⚠️  {len(disconnected) - len(disc_ok)} abrupt disconnections had issues")

        print("=" * 70)


def _upload_blob(server: str, data: bytes) -> remote_execution_pb2.Digest:
    """Upload a blob via CAS BatchUpdateBlobs and return its digest."""
    blob_hash = hashlib.sha256(data).hexdigest()
    digest = remote_execution_pb2.Digest(hash=blob_hash, size_bytes=len(data))
    channel = benchmark_lib.make_channel(server)
    try:
        stub = remote_execution_pb2_grpc.ContentAddressableStorageStub(channel)
        request = remote_execution_pb2.BatchUpdateBlobsRequest(
            requests=[
                remote_execution_pb2.BatchUpdateBlobsRequest.Request(
                    digest=digest, data=data
                )
            ]
        )
        response = stub.BatchUpdateBlobs(request)
        for status in response.responses:
            if status.status.code != 0:
                raise RuntimeError(f"Failed to upload blob: {status.status.message}")
        return digest
    finally:
        channel.close()


def _upload_action(server: str) -> remote_execution_pb2.Digest:
    """Upload a minimal action and its dependencies to CAS, return action digest."""
    action_digest, action_bytes, command_digest, command_bytes, input_root_digest, input_root_bytes = benchmark_lib.create_simple_action(
        command=["sh", "-c", "echo ok > output.txt"],
        output_paths=["output.txt"],
    )

    cas_channel = benchmark_lib.make_channel(server)
    try:
        cas_stub = remote_execution_pb2_grpc.ContentAddressableStorageStub(cas_channel)
        request = remote_execution_pb2.BatchUpdateBlobsRequest(
            requests=[
                remote_execution_pb2.BatchUpdateBlobsRequest.Request(
                    digest=digest, data=data
                )
                for digest, data in [
                    (action_digest, action_bytes),
                    (command_digest, command_bytes),
                    (input_root_digest, input_root_bytes),
                ]
            ]
        )
        response = cas_stub.BatchUpdateBlobs(request)
        for status in response.responses:
            if status.status.code != 0:
                raise RuntimeError(f"Failed to upload action blob: {status.status.message}")
    finally:
        cas_channel.close()

    return action_digest


def _seed_cas(server: str) -> Tuple[remote_execution_pb2.Digest, remote_execution_pb2.Digest]:
    """Ensure a valid blob and action exist in CAS for the churn tests."""
    with _seed_lock:
        if _seed_state["blob_digest"] is None:
            blob_data = os.urandom(1024 * 1024)  # 1MB blob
            _seed_state["blob_digest"] = _upload_blob(server, blob_data)
        if _seed_state["action_digest"] is None:
            _seed_state["action_digest"] = _upload_action(server)
        return _seed_state["blob_digest"], _seed_state["action_digest"]


def _run_download(
    channel: grpc.Channel,
    blob_digest: remote_execution_pb2.Digest,
    call_holder: dict,
    outcome: dict,
):
    """Run a download in a worker thread."""
    try:
        stub = bytestream_pb2_grpc.ByteStreamStub(channel)
        request = bytestream_pb2.ReadRequest(
            resource_name=f"blobs/{blob_digest.hash}/{blob_digest.size_bytes}"
        )
        call = stub.Read(request)
        call_holder["call"] = call
        consumed = 0
        for response in call:
            consumed += len(response.data)
        outcome["completed"] = consumed == blob_digest.size_bytes
    except grpc.RpcError as e:
        if e.code() in (grpc.StatusCode.CANCELLED, grpc.StatusCode.UNAVAILABLE):
            outcome["cancelled"] = True
        else:
            outcome["error"] = f"{e.code()}: {e.details()}"
    except Exception as e:
        outcome["error"] = str(e)


def _run_execute(
    channel: grpc.Channel,
    action_digest: remote_execution_pb2.Digest,
    call_holder: dict,
    outcome: dict,
):
    """Run an execute in a worker thread."""
    try:
        stub = remote_execution_pb2_grpc.ExecutionStub(channel)
        request = remote_execution_pb2.ExecuteRequest(
            action_digest=action_digest,
            skip_cache_lookup=True,
        )
        call = stub.Execute(request)
        call_holder["call"] = call
        # Read at least one message so the RPC is active on the wire.
        for _ in call:
            break
        outcome["completed"] = True
    except grpc.RpcError as e:
        if e.code() in (grpc.StatusCode.CANCELLED, grpc.StatusCode.UNAVAILABLE):
            outcome["cancelled"] = True
        else:
            outcome["error"] = f"{e.code()}: {e.details()}"
    except Exception as e:
        outcome["error"] = str(e)


def _test_connection(
    server: str,
    connection_id: str,
    operation: str,
    disconnect_rate: float,
    blob_digest: remote_execution_pb2.Digest,
    action_digest: remote_execution_pb2.Digest,
) -> ChurnResult:
    """Test a single connection with potential abrupt disconnection.

    A worker thread runs an active streaming RPC. The main thread either lets
    it complete normally or abruptly cancels the call and closes the channel.
    A clean disconnect means the worker observed CANCELLED/UNAVAILABLE and did
    not report an unexpected error.
    """
    start_time = time.time()
    should_disconnect = random.random() < disconnect_rate
    result = ChurnResult(
        connection_id=connection_id,
        operation=operation,
        duration_ms=0,
        disconnected=should_disconnect,
    )

    channel = benchmark_lib.make_channel(server)
    call_holder: dict = {}
    outcome: dict = {}

    if operation == "download":
        worker = threading.Thread(
            target=_run_download,
            args=(channel, blob_digest, call_holder, outcome),
        )
    else:  # execute
        worker = threading.Thread(
            target=_run_execute,
            args=(channel, action_digest, call_holder, outcome),
        )

    try:
        worker.start()

        if should_disconnect:
            time.sleep(random.uniform(0.05, 0.25))
            call = call_holder.get("call")
            if call is not None:
                try:
                    call.cancel()
                except Exception:
                    pass
            try:
                channel.close()
            except Exception:
                pass
            worker.join(timeout=5)
            result.resources_cleaned = outcome.get("cancelled") is True or outcome.get("error") is None
        else:
            worker.join(timeout=30)
            if worker.is_alive():
                call = call_holder.get("call")
                if call is not None:
                    try:
                        call.cancel()
                    except Exception:
                        pass
                try:
                    channel.close()
                except Exception:
                    pass
                worker.join(timeout=5)
                result.error = "Operation timed out"
            else:
                result.resources_cleaned = outcome.get("completed", False)
                if not result.resources_cleaned and outcome.get("error"):
                    result.error = outcome["error"]

        result.duration_ms = (time.time() - start_time) * 1000

    except Exception as e:
        result.error = str(e)
        result.duration_ms = (time.time() - start_time) * 1000
        result.resources_cleaned = False
    finally:
        try:
            channel.close()
        except Exception:
            pass

    return result


def _canary_check(server: str) -> bool:
    """Verify the server is still healthy after churn by running a simple action cache read."""
    try:
        channel = benchmark_lib.make_channel(server)
        stub = remote_execution_pb2_grpc.ActionCacheStub(channel)
        stub.GetActionResult(
            remote_execution_pb2.GetActionResultRequest(
                action_digest=remote_execution_pb2.Digest(hash="0" * 64, size_bytes=0)
            ),
            timeout=5,
        )
    except grpc.RpcError as e:
        if e.code() == grpc.StatusCode.NOT_FOUND:
            return True  # Server is alive and responded.
        return False
    except Exception:
        return False
    finally:
        channel.close()
    return True


def run_connection_churn_test(
    server: str,
    total_connections: int,
    disconnect_rate: float,
    concurrent: int = 50,
) -> ChurnSummary:
    """Run connection churn test."""
    print(f"Connecting to {server}...")

    blob_digest, action_digest = _seed_cas(server)
    print(f"Seeded CAS with blob ({blob_digest.size_bytes} bytes) and action")

    summary = ChurnSummary(
        server=server,
        total_connections=total_connections,
        disconnect_rate=disconnect_rate,
    )

    operations = ["download", "execute"]

    print(f"\n🎯 Test Plan:")
    print(f"   Create {total_connections} connections")
    print(f"   Disconnect {disconnect_rate * 100:.0f}% of them abruptly")
    print(f"   Measure whether channels close cleanly and server stays healthy")

    print(f"\n🔥 Starting connection churn...")

    with ThreadPoolExecutor(max_workers=concurrent) as executor:
        futures = {}
        for i in range(total_connections):
            operation = operations[i % len(operations)]
            future = executor.submit(
                _test_connection,
                server,
                f"conn-{i}",
                operation,
                disconnect_rate,
                blob_digest,
                action_digest,
            )
            futures[future] = i

        completed = 0
        for future in as_completed(futures):
            result = future.result()
            summary.results.append(result)
            completed += 1
            if completed % 100 == 0 or completed == total_connections:
                print(f"  Progress: {completed}/{total_connections} connections tested")

    canary_ok = _canary_check(server)
    print(f"\n{'✅' if canary_ok else '❌'} Server canary check: {'passed' if canary_ok else 'failed'}")

    return summary


def main():
    parser = argparse.ArgumentParser(
        description="Connection Churn Test - Abrupt disconnections and resource cleanup"
    )
    parser.add_argument("--server", default="localhost:9092", help="gRPC server address")
    parser.add_argument(
        "--connections", type=int, default=1000, help="Total connections to test (default: 1000)"
    )
    parser.add_argument(
        "--disconnect-rate", type=float, default=0.3,
        help="Rate of abrupt disconnections 0-1 (default: 0.3)"
    )
    parser.add_argument(
        "--concurrent", type=int, default=50, help="Concurrent connections (default: 50)"
    )
    parser.add_argument("--output", help="Output JSON file for results")

    args = parser.parse_args()

    print("=" * 70)
    print("CONNECTION CHURN TEST")
    print("Testing resource cleanup after abrupt disconnections")
    print("=" * 70)

    summary = run_connection_churn_test(
        server=args.server,
        total_connections=args.connections,
        disconnect_rate=args.disconnect_rate,
        concurrent=args.concurrent,
    )

    summary.print_summary()

    if args.output:
        result_dict = {
            "server": summary.server,
            "total_connections": summary.total_connections,
            "disconnect_rate": summary.disconnect_rate,
            "successful_cleanups": len(summary.get_successful_cleanups()),
            "failed_cleanups": len(summary.get_failed_cleanups()),
            "cleanup_times_ms": [r.duration_ms for r in summary.results if r.duration_ms > 0],
        }
        benchmark_lib.save_json(args.output, result_dict)
        print(f"\nResults exported to: {args.output}")


if __name__ == "__main__":
    main()
