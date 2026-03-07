#!/usr/bin/env python3
"""
Execution API Load Test for RBE Benchmarking
Tests concurrent action execution throughput and latency

This tests FerrisRBE's ability to handle thousands of concurrent Execute requests
without GC pauses (Rust) vs JVM-based solutions.
"""

import argparse
import asyncio
import hashlib
import os
import sys
import time
import statistics
from dataclasses import dataclass, field
from typing import List, Optional, Dict
from concurrent.futures import ThreadPoolExecutor, as_completed

import grpc

# Generated proto imports
sys.path.insert(0, os.path.join(os.path.dirname(__file__), '..', 'proto_gen'))

try:
    from build.bazel.remote.execution.v2 import remote_execution_pb2
    from build.bazel.remote.execution.v2 import remote_execution_pb2_grpc
    from google.longrunning import operations_pb2
    from google.longrunning import operations_pb2_grpc
except ImportError:
    print("Warning: Protocol buffer modules not found.")
    print("Install with: pip install grpcio grpcio-tools")
    print("Proto files need to be generated from the REAPI definitions.")
    sys.exit(1)


@dataclass
class ExecutionResult:
    """Single execution result"""
    action_id: str
    duration_ms: float
    success: bool
    error: Optional[str] = None
    queue_time_ms: Optional[float] = None


@dataclass
class ExecutionSummary:
    """Summary of execution benchmark"""
    server: str
    total_actions: int
    concurrent: int
    results: List[ExecutionResult] = field(default_factory=list)
    
    @property
    def success_count(self) -> int:
        return sum(1 for r in self.results if r.success)
    
    @property
    def fail_count(self) -> int:
        return sum(1 for r in self.results if not r.success)
    
    @property
    def latencies(self) -> List[float]:
        return [r.duration_ms for r in self.results if r.success]
    
    @property
    def throughput(self) -> float:
        """Actions per second"""
        if not self.latencies:
            return 0.0
        total_time = sum(self.latencies) / 1000  # Convert to seconds
        return len(self.latencies) / total_time if total_time > 0 else 0.0
    
    def print_summary(self):
        print("\n" + "=" * 70)
        print(f"EXECUTION BENCHMARK SUMMARY - {self.server}")
        print("=" * 70)
        print(f"Total actions: {self.total_actions}")
        print(f"Concurrency: {self.concurrent}")
        print(f"Success: {self.success_count} | Failed: {self.fail_count}")
        
        if self.latencies:
            print(f"\nLATENCY DISTRIBUTION:")
            print(f"  Min: {min(self.latencies):.2f} ms")
            print(f"  Max: {max(self.latencies):.2f} ms")
            print(f"  Mean: {statistics.mean(self.latencies):.2f} ms")
            print(f"  P50: {statistics.median(self.latencies):.2f} ms")
            print(f"  P95: {self._percentile(self.latencies, 95):.2f} ms")
            print(f"  P99: {self._percentile(self.latencies, 99):.2f} ms")
            print(f"\nTHROUGHPUT: {self.throughput:.2f} actions/second")
            
            # Calculate jitter (std deviation)
            if len(self.latencies) > 1:
                std_dev = statistics.stdev(self.latencies)
                print(f"JITTER (StdDev): {std_dev:.2f} ms")
        
        print("=" * 70)
    
    @staticmethod
    def _percentile(data: List[float], percentile: float) -> float:
        if not data:
            return 0.0
        sorted_data = sorted(data)
        index = int(len(sorted_data) * percentile / 100)
        return sorted_data[min(index, len(sorted_data) - 1)]


def create_simple_action(command: List[str]) -> tuple:
    """Create a simple action for testing"""
    # Create command proto
    command_proto = remote_execution_pb2.Command(
        arguments=command,
        output_paths=["output.txt"]
    )
    command_bytes = command_proto.SerializeToString()
    command_digest = remote_execution_pb2.Digest(
        hash=hashlib.sha256(command_bytes).hexdigest(),
        size_bytes=len(command_bytes)
    )
    
    # Create action proto
    action_proto = remote_execution_pb2.Action(
        command_digest=command_digest,
        do_not_cache=False
    )
    action_bytes = action_proto.SerializeToString()
    action_digest = remote_execution_pb2.Digest(
        hash=hashlib.sha256(action_bytes).hexdigest(),
        size_bytes=len(action_bytes)
    )
    
    return action_digest, action_bytes, command_digest, command_bytes


def execute_action(
    execution_stub: remote_execution_pb2_grpc.ExecutionStub,
    action_digest: remote_execution_pb2.Digest,
    action_id: str
) -> ExecutionResult:
    """Execute a single action and measure latency"""
    start = time.perf_counter()
    
    try:
        request = remote_execution_pb2.ExecuteRequest(
            action_digest=action_digest,
            skip_cache_lookup=False
        )
        
        # Start execution
        response_stream = execution_stub.Execute(request)
        
        # Wait for completion (for simple actions that complete immediately)
        # In real scenarios, this would be a long-running operation
        for operation in response_stream:
            if operation.done:
                break
        
        duration_ms = (time.perf_counter() - start) * 1000
        
        return ExecutionResult(
            action_id=action_id,
            duration_ms=duration_ms,
            success=True
        )
    
    except Exception as e:
        duration_ms = (time.perf_counter() - start) * 1000
        return ExecutionResult(
            action_id=action_id,
            duration_ms=duration_ms,
            success=False,
            error=str(e)
        )


def run_execution_load_test(
    server: str,
    num_actions: int,
    concurrent: int,
    command: List[str] = None
) -> ExecutionSummary:
    """Run complete execution load test"""
    
    if command is None:
        command = ["echo", "hello"]  # Simple fast action
    
    print(f"Connecting to {server}...")
    channel = grpc.insecure_channel(server)
    execution_stub = remote_execution_pb2_grpc.ExecutionStub(channel)
    
    # Create test action
    print(f"Creating test action: {' '.join(command)}")
    action_digest, action_bytes, cmd_digest, cmd_bytes = create_simple_action(command)
    
    # Upload action to CAS first (optional, depends on RBE implementation)
    cas_stub = remote_execution_pb2_grpc.ContentAddressableStorageStub(channel)
    try:
        cas_request = remote_execution_pb2.BatchUpdateBlobsRequest(
            requests=[
                remote_execution_pb2.BatchUpdateBlobsRequest.Request(
                    digest=action_digest,
                    data=action_bytes
                ),
                remote_execution_pb2.BatchUpdateBlobsRequest.Request(
                    digest=cmd_digest,
                    data=cmd_bytes
                )
            ]
        )
        cas_stub.BatchUpdateBlobs(cas_request)
    except Exception as e:
        print(f"Warning: Could not upload action to CAS: {e}")
    
    summary = ExecutionSummary(
        server=server,
        total_actions=num_actions,
        concurrent=concurrent
    )
    
    # Execution phase
    print(f"\nExecuting {num_actions} actions with concurrency {concurrent}...")
    print(f"Action: {' '.join(command)}")
    
    with ThreadPoolExecutor(max_workers=concurrent) as executor:
        futures = {
            executor.submit(execute_action, execution_stub, action_digest, f"action-{i}"): i
            for i in range(num_actions)
        }
        
        completed = 0
        for future in as_completed(futures):
            result = future.result()
            summary.results.append(result)
            completed += 1
            if completed % 100 == 0 or completed == num_actions:
                success_rate = summary.success_count / completed * 100
                print(f"  Completed {completed}/{num_actions} - Success: {success_rate:.1f}%")
    
    channel.close()
    return summary


def main():
    parser = argparse.ArgumentParser(description='Execution API Load Test for RBE')
    parser.add_argument('--server', default='localhost:9092', help='gRPC server address')
    parser.add_argument('--actions', type=int, default=1000, help='Number of actions to execute')
    parser.add_argument('--concurrent', type=int, default=50, help='Concurrent executions')
    parser.add_argument('--command', nargs='+', default=['echo', 'hello'],
                       help='Command to execute (default: echo hello)')
    parser.add_argument('--output', help='Output JSON file for results')
    
    args = parser.parse_args()
    
    # Run benchmark
    summary = run_execution_load_test(
        server=args.server,
        num_actions=args.actions,
        concurrent=args.concurrent,
        command=args.command
    )
    
    # Print summary
    summary.print_summary()
    
    # Export to JSON if requested
    if args.output:
        import json
        result_dict = {
            'server': summary.server,
            'total_actions': summary.total_actions,
            'concurrent': summary.concurrent,
            'success_count': summary.success_count,
            'fail_count': summary.fail_count,
            'throughput': summary.throughput,
            'latencies': {
                'min': min(summary.latencies) if summary.latencies else 0,
                'max': max(summary.latencies) if summary.latencies else 0,
                'mean': statistics.mean(summary.latencies) if summary.latencies else 0,
                'p50': statistics.median(summary.latencies) if summary.latencies else 0,
                'p95': summary._percentile(summary.latencies, 95) if summary.latencies else 0,
                'p99': summary._percentile(summary.latencies, 99) if summary.latencies else 0,
                'stddev': statistics.stdev(summary.latencies) if len(summary.latencies) > 1 else 0,
            }
        }
        with open(args.output, 'w') as f:
            json.dump(result_dict, f, indent=2)
        print(f"\nResults exported to: {args.output}")


if __name__ == '__main__':
    main()
