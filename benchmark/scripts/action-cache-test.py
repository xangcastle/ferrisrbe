#!/usr/bin/env python3
"""
Action Cache Load Test for RBE Benchmarking
Tests concurrent Action Cache read performance

This demonstrates FerrisRBE's advantage with DashMap (lock-free concurrent hash map)
vs Redis-based (Buildfarm) or database-backed (Buildbarn) solutions.
"""

import argparse
import asyncio
import hashlib
import os
import sys
import time
import statistics
from dataclasses import dataclass, field
from typing import List, Optional
from concurrent.futures import ThreadPoolExecutor, as_completed

import grpc

sys.path.insert(0, os.path.join(os.path.dirname(__file__), '..', 'proto_gen'))

try:
    from build.bazel.remote.execution.v2 import remote_execution_pb2
    from build.bazel.remote.execution.v2 import remote_execution_pb2_grpc
except ImportError:
    print("Warning: Protocol buffer modules not found.")
    sys.exit(1)


@dataclass
class CacheResult:
    """Single cache operation result"""
    operation: str  # 'read' or 'write'
    duration_us: float  # Microseconds for precision
    success: bool
    hit: bool  # For reads: was it a cache hit?
    error: Optional[str] = None


@dataclass
class CacheSummary:
    """Summary of cache benchmark"""
    server: str
    operation: str
    total_operations: int
    concurrent: int
    results: List[CacheResult] = field(default_factory=list)
    
    @property
    def success_count(self) -> int:
        return sum(1 for r in self.results if r.success)
    
    @property
    def hit_count(self) -> int:
        return sum(1 for r in self.results if r.hit)
    
    @property
    def latencies_us(self) -> List[float]:
        return [r.duration_us for r in self.results if r.success]
    
    @property
    def throughput(self) -> float:
        """Operations per second"""
        if not self.latencies_us:
            return 0.0
        total_time_us = sum(self.latencies_us)
        return len(self.latencies_us) / (total_time_us / 1_000_000) if total_time_us > 0 else 0.0
    
    def print_summary(self):
        print("\n" + "=" * 70)
        print(f"ACTION CACHE BENCHMARK - {self.operation.upper()} - {self.server}")
        print("=" * 70)
        print(f"Total operations: {self.total_operations}")
        print(f"Concurrency: {self.concurrent}")
        print(f"Success: {self.success_count}")
        if self.operation == 'read':
            print(f"Cache hits: {self.hit_count} ({self.hit_count/self.success_count*100:.1f}%)")
        
        if self.latencies_us:
            latencies_ms = [us / 1000 for us in self.latencies_us]
            print(f"\nLATENCY DISTRIBUTION (microseconds):")
            print(f"  Min: {min(self.latencies_us):.0f} μs ({min(latencies_ms):.3f} ms)")
            print(f"  Max: {max(self.latencies_us):.0f} μs ({max(latencies_ms):.3f} ms)")
            print(f"  Mean: {statistics.mean(self.latencies_us):.0f} μs ({statistics.mean(latencies_ms):.3f} ms)")
            print(f"  P50: {statistics.median(self.latencies_us):.0f} μs ({statistics.median(latencies_ms):.3f} ms)")
            print(f"  P95: {self._percentile(self.latencies_us, 95):.0f} μs")
            print(f"  P99: {self._percentile(self.latencies_us, 99):.0f} μs")
            
            if len(self.latencies_us) > 1:
                std_dev = statistics.stdev(self.latencies_us)
                print(f"  StdDev: {std_dev:.0f} μs")
            
            print(f"\nTHROUGHPUT: {self.throughput:,.0f} ops/second")
            
            # Microseconds per operation
            avg_us = statistics.mean(self.latencies_us)
            if avg_us < 1000:
                print(f"AVG LATENCY: {avg_us:.0f} μs")
            else:
                print(f"AVG LATENCY: {avg_us/1000:.2f} ms")
        
        print("=" * 70)
    
    @staticmethod
    def _percentile(data: List[float], percentile: float) -> float:
        if not data:
            return 0.0
        sorted_data = sorted(data)
        index = int(len(sorted_data) * percentile / 100)
        return sorted_data[min(index, len(sorted_data) - 1)]


def create_test_action_result(action_digest_hash: str) -> remote_execution_pb2.ActionResult:
    """Create a test action result"""
    return remote_execution_pb2.ActionResult(
        exit_code=0,
        stdout_raw=b"Hello from benchmark",
        stdout_digest=remote_execution_pb2.Digest(
            hash=hashlib.sha256(b"Hello from benchmark").hexdigest(),
            size_bytes=len(b"Hello from benchmark")
        )
    )


def write_action_result(
    ac_stub: remote_execution_pb2_grpc.ActionCacheStub,
    action_digest: remote_execution_pb2.Digest,
    result: remote_execution_pb2.ActionResult
) -> CacheResult:
    """Write action result to cache"""
    start = time.perf_counter()
    
    try:
        request = remote_execution_pb2.UpdateActionResultRequest(
            action_digest=action_digest,
            action_result=result
        )
        ac_stub.UpdateActionResult(request)
        
        duration_us = (time.perf_counter() - start) * 1_000_000
        
        return CacheResult(
            operation='write',
            duration_us=duration_us,
            success=True,
            hit=False
        )
    except Exception as e:
        duration_us = (time.perf_counter() - start) * 1_000_000
        return CacheResult(
            operation='write',
            duration_us=duration_us,
            success=False,
            hit=False,
            error=str(e)
        )


def read_action_result(
    ac_stub: remote_execution_pb2_grpc.ActionCacheStub,
    action_digest: remote_execution_pb2.Digest
) -> CacheResult:
    """Read action result from cache"""
    start = time.perf_counter()
    
    try:
        request = remote_execution_pb2.GetActionResultRequest(
            action_digest=action_digest
        )
        response = ac_stub.GetActionResult(request)
        
        duration_us = (time.perf_counter() - start) * 1_000_000
        
        return CacheResult(
            operation='read',
            duration_us=duration_us,
            success=True,
            hit=True
        )
    
    except grpc.RpcError as e:
        duration_us = (time.perf_counter() - start) * 1_000_000
        # NOT_FOUND is expected for cache misses
        if e.code() == grpc.StatusCode.NOT_FOUND:
            return CacheResult(
                operation='read',
                duration_us=duration_us,
                success=True,
                hit=False
            )
        return CacheResult(
            operation='read',
            duration_us=duration_us,
            success=False,
            hit=False,
            error=str(e)
        )
    
    except Exception as e:
        duration_us = (time.perf_counter() - start) * 1_000_000
        return CacheResult(
            operation='read',
            duration_us=duration_us,
            success=False,
            hit=False,
            error=str(e)
        )


def run_cache_benchmark(
    server: str,
    num_operations: int,
    concurrent: int,
    operation: str = 'read',
    prepopulate: bool = True
) -> CacheSummary:
    """Run action cache benchmark"""
    
    print(f"Connecting to {server}...")
    channel = grpc.insecure_channel(server)
    ac_stub = remote_execution_pb2_grpc.ActionCacheStub(channel)
    
    summary = CacheSummary(
        server=server,
        operation=operation,
        total_operations=num_operations,
        concurrent=concurrent
    )
    
    # Generate test action digests
    print(f"Generating {num_operations} test action digests...")
    action_digests = []
    for i in range(num_operations):
        action_hash = hashlib.sha256(f"action-{i}-{time.time()}".encode()).hexdigest()
        action_digests.append(remote_execution_pb2.Digest(
            hash=action_hash,
            size_bytes=100
        ))
    
    # Pre-populate cache for read tests
    if operation == 'read' and prepopulate:
        print(f"Pre-populating cache with {num_operations} entries...")
        for i, digest in enumerate(action_digests):
            result = create_test_action_result(digest.hash)
            write_action_result(ac_stub, digest, result)
            if (i + 1) % 100 == 0:
                print(f"  Written {i + 1}/{num_operations} entries...")
    
    # Run benchmark
    print(f"\nRunning {num_operations} {operation} operations with concurrency {concurrent}...")
    
    with ThreadPoolExecutor(max_workers=concurrent) as executor:
        if operation == 'read':
            futures = {
                executor.submit(read_action_result, ac_stub, digest): digest
                for digest in action_digests
            }
        else:  # write
            results = [create_test_action_result(d.hash) for d in action_digests]
            futures = {
                executor.submit(write_action_result, ac_stub, digest, result): digest
                for digest, result in zip(action_digests, results)
            }
        
        completed = 0
        for future in as_completed(futures):
            result = future.result()
            summary.results.append(result)
            completed += 1
            if completed % 1000 == 0 or completed == num_operations:
                print(f"  Completed {completed}/{num_operations} operations...")
    
    channel.close()
    return summary


def main():
    parser = argparse.ArgumentParser(description='Action Cache Load Test')
    parser.add_argument('--server', default='localhost:9092', help='gRPC server address')
    parser.add_argument('--operations', type=int, default=10000, help='Number of operations')
    parser.add_argument('--concurrent', type=int, default=100, help='Concurrent operations')
    parser.add_argument('--operation', choices=['read', 'write'], default='read',
                       help='Type of operation to benchmark')
    parser.add_argument('--no-prepopulate', action='store_true',
                       help='Skip cache pre-population (for read tests)')
    parser.add_argument('--output', help='Output JSON file for results')
    
    args = parser.parse_args()
    
    # Run benchmark
    summary = run_cache_benchmark(
        server=args.server,
        num_operations=args.operations,
        concurrent=args.concurrent,
        operation=args.operation,
        prepopulate=not args.no_prepopulate
    )
    
    # Print summary
    summary.print_summary()
    
    # Export if requested
    if args.output:
        import json
        result_dict = {
            'server': summary.server,
            'operation': summary.operation,
            'total_operations': summary.total_operations,
            'concurrent': summary.concurrent,
            'success_count': summary.success_count,
            'hit_count': summary.hit_count,
            'throughput': summary.throughput,
            'latencies_us': {
                'min': min(summary.latencies_us) if summary.latencies_us else 0,
                'max': max(summary.latencies_us) if summary.latencies_us else 0,
                'mean': statistics.mean(summary.latencies_us) if summary.latencies_us else 0,
                'p50': statistics.median(summary.latencies_us) if summary.latencies_us else 0,
                'p95': summary._percentile(summary.latencies_us, 95) if summary.latencies_us else 0,
                'p99': summary._percentile(summary.latencies_us, 99) if summary.latencies_us else 0,
            }
        }
        with open(args.output, 'w') as f:
            json.dump(result_dict, f, indent=2)
        print(f"\nResults exported to: {args.output}")


if __name__ == '__main__':
    main()
