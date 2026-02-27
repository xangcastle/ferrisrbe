#!/usr/bin/env python3
"""
Cache Stampede (Thundering Herd) Test
Simulates thousands of simultaneous requests for the same uncached action

FerrisRBE Advantage: DashMap + request coalescing prevents overload.
Redis/DB-backed solutions may be overwhelmed by identical key lookups.
"""

import argparse
import hashlib
import os
import sys
import time
import statistics
from dataclasses import dataclass, field
from typing import List, Optional, Dict
from concurrent.futures import ThreadPoolExecutor, as_completed
import threading

import grpc

sys.path.insert(0, os.path.join(os.path.dirname(__file__), '..', 'proto_gen'))

try:
    from build.bazel.remote.execution.v2 import remote_execution_pb2
    from build.bazel.remote.execution.v2 import remote_execution_pb2_grpc
except ImportError:
    print("Warning: Protocol buffer modules not found.")
    sys.exit(1)


@dataclass
class StampedeResult:
    """Result of a single request during stampede"""
    request_id: str
    duration_ms: float
    cache_hit: bool
    coalesced: bool = False  # If request was coalesced with another
    error: Optional[str] = None


@dataclass
class StampedeSummary:
    """Summary of cache stampede test"""
    server: str
    total_requests: int
    concurrent: int
    results: List[StampedeResult] = field(default_factory=list)
    
    def get_hits(self) -> List[StampedeResult]:
        return [r for r in self.results if r.cache_hit]
    
    def get_misses(self) -> List[StampedeResult]:
        return [r for r in self.results if not r.cache_hit and not r.error]
    
    def get_errors(self) -> List[StampedeResult]:
        return [r for r in self.results if r.error]
    
    def get_latencies(self) -> List[float]:
        return [r.duration_ms for r in self.results if not r.error]
    
    def print_summary(self):
        print("\n" + "=" * 70)
        print(f"CACHE STAMPEDE (THUNDERING HERD) TEST - {self.server}")
        print("=" * 70)
        print(f"\n📊 TEST PARAMETERS:")
        print(f"  Total requests: {self.total_requests}")
        print(f"  Concurrency: {self.concurrent}")
        print(f"  All requesting: SAME action digest (uncached)")
        
        hits = len(self.get_hits())
        misses = len(self.get_misses())
        errors = len(self.get_errors())
        latencies = self.get_latencies()
        
        print(f"\n📊 RESULTS:")
        print(f"  Cache hits: {hits} ({hits/len(self.results)*100:.1f}%)")
        print(f"  Cache misses: {misses} ({misses/len(self.results)*100:.1f}%)")
        print(f"  Errors: {errors} ({errors/len(self.results)*100:.1f}%)")
        
        if latencies:
            print(f"\n📊 LATENCY DISTRIBUTION:")
            print(f"  Min: {min(latencies):.2f}ms")
            print(f"  Mean: {statistics.mean(latencies):.2f}ms")
            print(f"  P50: {statistics.median(latencies):.2f}ms")
            print(f"  P95: {self._percentile(latencies, 95):.2f}ms")
            print(f"  P99: {self._percentile(latencies, 99):.2f}ms")
            print(f"  Max: {max(latencies):.2f}ms")
            
            if len(latencies) > 1:
                print(f"  StdDev: {statistics.stdev(latencies):.2f}ms")
        
        # Check for stampede protection
        if latencies:
            # If P99 is not much higher than mean, likely coalesced
            p99 = self._percentile(latencies, 99)
            mean = statistics.mean(latencies)
            ratio = p99 / mean if mean > 0 else 0
            
            print(f"\n🏆 STAMPEDE PROTECTION ANALYSIS:")
            print(f"  P99/Mean ratio: {ratio:.2f}x")
            
            if ratio < 2.0 and errors == 0:
                print(f"  ✅ EXCELLENT: Request coalescing detected")
                print(f"     (All requests served efficiently)")
            elif ratio < 3.0 and errors < self.total_requests * 0.01:
                print(f"  ✅ GOOD: Some coalescing or fast backend")
            elif errors > self.total_requests * 0.05:
                print(f"  ❌ CRITICAL: Backend overwhelmed")
                print(f"     (Consider request coalescing)")
            else:
                print(f"  ⚠️  WARNING: High latency variance")
                print(f"     (Possible backend contention)")
        
        print("=" * 70)
    
    @staticmethod
    def _percentile(data: List[float], percentile: float) -> float:
        if not data:
            return 0.0
        sorted_data = sorted(data)
        index = int(len(sorted_data) * percentile / 100)
        return sorted_data[min(index, len(sorted_data) - 1)]


def make_action_cache_request(
    ac_stub: remote_execution_pb2_grpc.ActionCacheStub,
    action_digest: remote_execution_pb2.Digest,
    request_id: str,
    barrier: threading.Barrier = None
) -> StampedeResult:
    """Make a single action cache request, optionally synchronized"""
    
    # Wait at barrier if provided (for simultaneous launch)
    if barrier:
        barrier.wait()
    
    start = time.perf_counter()
    
    try:
        request = remote_execution_pb2.GetActionResultRequest(
            action_digest=action_digest
        )
        
        response = ac_stub.GetActionResult(request)
        
        duration_ms = (time.perf_counter() - start) * 1000
        
        # If we got a response, it's a cache hit
        return StampedeResult(
            request_id=request_id,
            duration_ms=duration_ms,
            cache_hit=True
        )
    
    except grpc.RpcError as e:
        duration_ms = (time.perf_counter() - start) * 1000
        
        if e.code() == grpc.StatusCode.NOT_FOUND:
            # Cache miss - this is expected for uncached actions
            return StampedeResult(
                request_id=request_id,
                duration_ms=duration_ms,
                cache_hit=False
            )
        else:
            # Other error
            return StampedeResult(
                request_id=request_id,
                duration_ms=duration_ms,
                cache_hit=False,
                error=f"{e.code()}: {e.details()}"
            )
    
    except Exception as e:
        duration_ms = (time.perf_counter() - start) * 1000
        return StampedeResult(
            request_id=request_id,
            duration_ms=duration_ms,
            cache_hit=False,
            error=str(e)
        )


def run_cache_stampede_test(
    server: str,
    total_requests: int,
    concurrent: int,
    simultaneous: bool = True
) -> StampedeSummary:
    """
    Run cache stampede test
    
    Strategy:
    1. Generate a single action digest that is NOT in cache
    2. Launch thousands of concurrent requests for that same digest
    3. Measure if the backend handles it efficiently
    """
    
    print(f"Connecting to {server}...")
    channel = grpc.insecure_channel(server)
    ac_stub = remote_execution_pb2_grpc.ActionCacheStub(channel)
    
    summary = StampedeSummary(
        server=server,
        total_requests=total_requests,
        concurrent=concurrent
    )
    
    # Generate a unique action digest (won't be in cache)
    unique_data = f"stampede-test-{time.time()}-{os.urandom(16).hex()}"
    action_hash = hashlib.sha256(unique_data.encode()).hexdigest()
    action_digest = remote_execution_pb2.Digest(
        hash=action_hash,
        size_bytes=len(unique_data)
    )
    
    print(f"\n🎯 Test Plan:")
    print(f"   Launch {total_requests} requests")
    print(f"   ALL for same action digest: {action_hash[:16]}...")
    print(f"   Concurrency: {concurrent}")
    print(f"   Simultaneous: {'YES' if simultaneous else 'NO'}")
    
    print(f"\n🔥 TRIGGERING STAMPEDE...")
    
    # Use barrier for simultaneous launch
    barrier = threading.Barrier(concurrent) if simultaneous else None
    
    with ThreadPoolExecutor(max_workers=concurrent) as executor:
        futures = {}
        for i in range(total_requests):
            future = executor.submit(
                make_action_cache_request,
                ac_stub,
                action_digest,
                f"req-{i}",
                barrier if i < concurrent else None  # Only first batch uses barrier
            )
            futures[future] = i
        
        completed = 0
        for future in as_completed(futures):
            result = future.result()
            summary.results.append(result)
            completed += 1
            
            if completed % 1000 == 0 or completed == total_requests:
                print(f"  Progress: {completed}/{total_requests} requests completed")
    
    channel.close()
    return summary


def run_coalescing_test(
    server: str,
    total_requests: int,
    concurrent: int
) -> StampedeSummary:
    """
    Test specifically for request coalescing
    First request should trigger backend lookup, subsequent should wait and reuse result
    """
    
    print(f"\n🔄 Testing Request Coalescing...")
    print(f"   {total_requests} requests for same digest")
    print(f"   Measuring if subsequent requests benefit from first")
    
    channel = grpc.insecure_channel(server)
    ac_stub = remote_execution_pb2_grpc.ActionCacheStub(channel)
    
    summary = StampedeSummary(
        server=server,
        total_requests=total_requests,
        concurrent=concurrent
    )
    
    # Unique digest
    unique_data = f"coalesce-test-{time.time()}"
    action_hash = hashlib.sha256(unique_data.encode()).hexdigest()
    action_digest = remote_execution_pb2.Digest(
        hash=action_hash,
        size_bytes=len(unique_data)
    )
    
    # Pre-populate cache with this entry (so all should be hits)
    # This lets us measure if concurrent reads are efficient
    result = remote_execution_pb2.ActionResult(
        exit_code=0,
        stdout_raw=b"test"
    )
    try:
        ac_stub.UpdateActionResult(
            remote_execution_pb2.UpdateActionResultRequest(
                action_digest=action_digest,
                action_result=result
            )
        )
    except:
        pass  # May fail if not supported
    
    # Now hit it with concurrent requests
    with ThreadPoolExecutor(max_workers=concurrent) as executor:
        barrier = threading.Barrier(concurrent)
        futures = {
            executor.submit(
                make_action_cache_request,
                ac_stub,
                action_digest,
                f"coalesce-{i}",
                barrier
            ): i
            for i in range(total_requests)
        }
        
        for future in as_completed(futures):
            result = future.result()
            summary.results.append(result)
    
    channel.close()
    return summary


def main():
    parser = argparse.ArgumentParser(
        description='Cache Stampede (Thundering Herd) Test'
    )
    parser.add_argument('--server', default='localhost:9092', help='gRPC server address')
    parser.add_argument('--requests', type=int, default=10000,
                       help='Total requests (default: 10000)')
    parser.add_argument('--concurrent', type=int, default=100,
                       help='Concurrent requests (default: 100)')
    parser.add_argument('--no-simultaneous', action='store_true',
                       help='Disable simultaneous launch (staggered requests)')
    parser.add_argument('--coalescing-test', action='store_true',
                       help='Run coalescing test after stampede test')
    parser.add_argument('--output', help='Output JSON file for results')
    
    args = parser.parse_args()
    
    print("=" * 70)
    print("CACHE STAMPEDE (THUNDERING HERD) TEST")
    print("Tests handling of simultaneous requests for same uncached key")
    print("=" * 70)
    
    # Run stampede test
    summary = run_cache_stampede_test(
        server=args.server,
        total_requests=args.requests,
        concurrent=args.concurrent,
        simultaneous=not args.no_simultaneous
    )
    
    # Print summary
    summary.print_summary()
    
    # Run coalescing test if requested
    if args.coalescing_test:
        print("\n" + "=" * 70)
        coalesce_summary = run_coalescing_test(
            server=args.server,
            total_requests=args.requests,
            concurrent=args.concurrent
        )
        coalesce_summary.print_summary()
    
    # Export if requested
    if args.output:
        import json
        result_dict = {
            'server': summary.server,
            'total_requests': summary.total_requests,
            'concurrent': summary.concurrent,
            'cache_hits': len(summary.get_hits()),
            'cache_misses': len(summary.get_misses()),
            'errors': len(summary.get_errors()),
            'latencies_ms': {
                'min': min(summary.get_latencies()) if summary.get_latencies() else 0,
                'max': max(summary.get_latencies()) if summary.get_latencies() else 0,
                'mean': statistics.mean(summary.get_latencies()) if summary.get_latencies() else 0,
                'p50': statistics.median(summary.get_latencies()) if summary.get_latencies() else 0,
                'p95': summary._percentile(summary.get_latencies(), 95) if summary.get_latencies() else 0,
                'p99': summary._percentile(summary.get_latencies(), 99) if summary.get_latencies() else 0,
            }
        }
        with open(args.output, 'w') as f:
            json.dump(result_dict, f, indent=2)
        print(f"\nResults exported to: {args.output}")


if __name__ == '__main__':
    main()
