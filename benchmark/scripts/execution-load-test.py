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
    action_id: str,
    timeout_secs: int = 60
) -> ExecutionResult:
    """Execute a single action with timeout protection"""
    start = time.perf_counter()
    
    try:
        request = remote_execution_pb2.ExecuteRequest(
            action_digest=action_digest,
            skip_cache_lookup=False
        )
        
        # Start execution with timeout
        response_stream = execution_stub.Execute(request, timeout=timeout_secs)
        
        # Wait for completion with deadline check
        deadline = time.perf_counter() + timeout_secs
        for operation in response_stream:
            if operation.done:
                break
            if time.perf_counter() > deadline:
                raise TimeoutError(f"Execution timed out after {timeout_secs}s")
        
        duration_ms = (time.perf_counter() - start) * 1000
        
        return ExecutionResult(
            action_id=action_id,
            duration_ms=duration_ms,
            success=True
        )
    
    except grpc.RpcError as e:
        duration_ms = (time.perf_counter() - start) * 1000
        if e.code() == grpc.StatusCode.DEADLINE_EXCEEDED:
            return ExecutionResult(
                action_id=action_id,
                duration_ms=duration_ms,
                success=False,
                error=f"Timeout: execution exceeded {timeout_secs}s time limit"
            )
        return ExecutionResult(
            action_id=action_id,
            duration_ms=duration_ms,
            success=False,
            error=f"RPC error: {e.code()} - {e.details()}"
        )
    
    except TimeoutError as e:
        duration_ms = (time.perf_counter() - start) * 1000
        return ExecutionResult(
            action_id=action_id,
            duration_ms=duration_ms,
            success=False,
            error=str(e)
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
    command: List[str] = None,
    timeout_per_action: int = 60,
    test_timeout_multiplier: int = 5
) -> ExecutionSummary:
    """Run complete execution load test with timeout protection"""
    
    if command is None:
        command = ["echo", "hello"]  # Simple fast action
    
    print(f"Connecting to {server}...")
    
    # Configure channel with keepalive timeouts
    # Using relaxed settings for CI environment to reduce network overhead
    channel_options = [
        ('grpc.keepalive_time_ms', 30000),      # 30 seconds
        ('grpc.keepalive_timeout_ms', 10000),   # 10 seconds timeout
        ('grpc.http2.max_pings_without_data', 0),
        ('grpc.http2.min_time_between_pings_ms', 30000),
    ]
    channel = grpc.insecure_channel(server, options=channel_options)
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
    print(f"Timeout per action: {timeout_per_action}s")
    
    # Calculate global test timeout (test_timeout_multiplier seconds per action max)
    global_timeout = num_actions * test_timeout_multiplier
    print(f"Global test timeout: {global_timeout}s")
    
    with ThreadPoolExecutor(max_workers=concurrent) as executor:
        futures = {
            executor.submit(
                execute_action,
                execution_stub,
                action_digest,
                f"action-{i}",
                timeout_per_action
            ): i
            for i in range(num_actions)
        }
        
        completed = 0
        start_time = time.time()
        global_timeout_reached = False
        
        for future in as_completed(futures):
            # Check global timeout
            if not global_timeout_reached and (time.time() - start_time) > global_timeout:
                global_timeout_reached = True
                print(f"\n  WARNING: Global test timeout ({global_timeout}s) reached")
                print(f"  Completed {completed}/{num_actions} actions so far")
                # Cancel remaining futures
                for f in futures:
                    if not f.done():
                        f.cancel()
            
            try:
                # Get result with timeout per future
                result = future.result(timeout=timeout_per_action)
                summary.results.append(result)
            except Exception as e:
                # Handle timeout or other errors
                action_id = f"action-{futures[future]}"
                summary.results.append(ExecutionResult(
                    action_id=action_id,
                    duration_ms=0,
                    success=False,
                    error=f"Future error: {str(e)}"
                ))
            
            completed += 1
            if completed % 100 == 0 or completed == num_actions:
                success_rate = summary.success_count / completed * 100
                print(f"  Completed {completed}/{num_actions} - Success: {success_rate:.1f}%")
                
                if global_timeout_reached:
                    break
        
        # Mark remaining actions as failed if timeout was reached
        if global_timeout_reached:
            remaining = num_actions - len(summary.results)
            for i in range(len(summary.results), num_actions):
                summary.results.append(ExecutionResult(
                    action_id=f"action-{i}",
                    duration_ms=0,
                    success=False,
                    error=f"Cancelled: global timeout ({global_timeout}s) reached"
                ))
            print(f"  Marked {remaining} remaining actions as failed due to timeout")
    
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
