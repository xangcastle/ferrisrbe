#!/usr/bin/env python3
"""
CAS Load Test Script for RBE Benchmarking
Generates synthetic load to test memory usage and latency

Usage:
    python cas-load-test.py --server localhost:9092 --blobs 100 --size 1048576
"""

import argparse
import hashlib
import os
import random
import string
import sys
import time
import statistics
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass, field
from typing import List, Optional

import grpc

# Add generated proto path
sys.path.insert(0, os.path.join(os.path.dirname(__file__), '..', 'proto_gen'))

try:
    from build.bazel.remote.execution.v2 import remote_execution_pb2
    from build.bazel.remote.execution.v2 import remote_execution_pb2_grpc
except ImportError:
    print("Warning: Protocol buffer modules not found. Using mock implementations.")
    print("Install with: pip install grpcio grpcio-tools")
    sys.exit(1)


@dataclass
class BenchmarkResult:
    """Single benchmark result"""
    operation: str  # 'upload' or 'download'
    blob_size: int
    duration_ms: float
    success: bool
    error: Optional[str] = None


@dataclass
class BenchmarkSummary:
    """Summary of benchmark results"""
    server: str
    total_blobs: int
    blob_size: int
    concurrent: int
    results: List[BenchmarkResult] = field(default_factory=list)
    
    @property
    def success_count(self) -> int:
        return sum(1 for r in self.results if r.success)
    
    @property
    def fail_count(self) -> int:
        return sum(1 for r in self.results if not r.success)
    
    @property
    def total_bytes(self) -> int:
        return sum(r.blob_size for r in self.results if r.success)
    
    @property
    def upload_latencies(self) -> List[float]:
        return [r.duration_ms for r in self.results if r.operation == 'upload' and r.success]
    
    @property
    def download_latencies(self) -> List[float]:
        return [r.duration_ms for r in self.results if r.operation == 'download' and r.success]
    
    def print_summary(self):
        print("\n" + "=" * 60)
        print(f"BENCHMARK SUMMARY - {self.server}")
        print("=" * 60)
        print(f"Total blobs: {self.total_blobs} x {self.blob_size / 1024 / 1024:.1f}MB = {self.total_blobs * self.blob_size / 1024 / 1024 / 1024:.1f}GB")
        print(f"Concurrency: {self.concurrent}")
        print(f"Success: {self.success_count} | Failed: {self.fail_count}")
        print(f"Total data transferred: {self.total_bytes / 1024 / 1024:.1f} MB")
        
        if self.upload_latencies:
            print(f"\nUPLOAD LATENCIES:")
            print(f"  Min: {min(self.upload_latencies):.2f} ms")
            print(f"  Max: {max(self.upload_latencies):.2f} ms")
            print(f"  Mean: {statistics.mean(self.upload_latencies):.2f} ms")
            print(f"  P50: {statistics.median(self.upload_latencies):.2f} ms")
            print(f"  P95: {self._percentile(self.upload_latencies, 95):.2f} ms")
            print(f"  P99: {self._percentile(self.upload_latencies, 99):.2f} ms")
        
        if self.download_latencies:
            print(f"\nDOWNLOAD LATENCIES:")
            print(f"  Min: {min(self.download_latencies):.2f} ms")
            print(f"  Max: {max(self.download_latencies):.2f} ms")
            print(f"  Mean: {statistics.mean(self.download_latencies):.2f} ms")
            print(f"  P50: {statistics.median(self.download_latencies):.2f} ms")
            print(f"  P95: {self._percentile(self.download_latencies, 95):.2f} ms")
            print(f"  P99: {self._percentile(self.download_latencies, 99):.2f} ms")
        
        print("=" * 60)
    
    @staticmethod
    def _percentile(data: List[float], percentile: float) -> float:
        """Calculate percentile"""
        if not data:
            return 0.0
        sorted_data = sorted(data)
        index = int(len(sorted_data) * percentile / 100)
        return sorted_data[min(index, len(sorted_data) - 1)]


def generate_blob(size: int) -> bytes:
    """Generate random blob of specified size"""
    # Use random bytes for better compression resistance
    return os.urandom(size)


def compute_digest(data: bytes) -> tuple:
    """Compute SHA256 digest and return (hash, size)"""
    hash_bytes = hashlib.sha256(data).digest()
    hash_hex = hash_bytes.hex()
    return hash_hex, len(data)


def upload_blob_batch(
    cas_stub: remote_execution_pb2_grpc.ContentAddressableStorageStub,
    data: bytes,
    hash_hex: str
) -> BenchmarkResult:
    """Upload blob using BatchUpdateBlobs"""
    start = time.perf_counter()
    
    try:
        digest = remote_execution_pb2.Digest(hash=hash_hex, size_bytes=len(data))
        request = remote_execution_pb2.BatchUpdateBlobsRequest(
            requests=[
                remote_execution_pb2.BatchUpdateBlobsRequest.Request(
                    digest=digest,
                    data=data
                )
            ]
        )
        response = cas_stub.BatchUpdateBlobs(request)
        
        duration_ms = (time.perf_counter() - start) * 1000
        
        # Check for errors
        if response.responses and response.responses[0].status.code != 0:
            return BenchmarkResult(
                operation='upload',
                blob_size=len(data),
                duration_ms=duration_ms,
                success=False,
                error=f"Status code: {response.responses[0].status.code}"
            )
        
        return BenchmarkResult(
            operation='upload',
            blob_size=len(data),
            duration_ms=duration_ms,
            success=True
        )
    except Exception as e:
        duration_ms = (time.perf_counter() - start) * 1000
        return BenchmarkResult(
            operation='upload',
            blob_size=len(data),
            duration_ms=duration_ms,
            success=False,
            error=str(e)
        )


def download_blob_batch(
    cas_stub: remote_execution_pb2_grpc.ContentAddressableStorageStub,
    hash_hex: str,
    size: int
) -> BenchmarkResult:
    """Download blob using BatchReadBlobs"""
    start = time.perf_counter()
    
    try:
        digest = remote_execution_pb2.Digest(hash=hash_hex, size_bytes=size)
        request = remote_execution_pb2.BatchReadBlobsRequest(
            digests=[digest]
        )
        response = cas_stub.BatchReadBlobs(request)
        
        duration_ms = (time.perf_counter() - start) * 1000
        
        # Check for errors
        if response.responses and response.responses[0].status.code != 0:
            return BenchmarkResult(
                operation='download',
                blob_size=size,
                duration_ms=duration_ms,
                success=False,
                error=f"Status code: {response.responses[0].status.code}"
            )
        
        return BenchmarkResult(
            operation='download',
            blob_size=size,
            duration_ms=duration_ms,
            success=True
        )
    except Exception as e:
        duration_ms = (time.perf_counter() - start) * 1000
        return BenchmarkResult(
            operation='download',
            blob_size=size,
            duration_ms=duration_ms,
            success=False,
            error=str(e)
        )


def run_load_test(
    server: str,
    num_blobs: int,
    blob_size: int,
    concurrent: int
) -> BenchmarkSummary:
    """Run complete load test"""
    
    print(f"Connecting to {server}...")
    channel = grpc.insecure_channel(server)
    cas_stub = remote_execution_pb2_grpc.ContentAddressableStorageStub(channel)
    
    # Generate test data
    print(f"Generating {num_blobs} blobs of {blob_size / 1024 / 1024:.1f}MB each...")
    test_data = []
    for i in range(num_blobs):
        data = generate_blob(blob_size)
        hash_hex, _ = compute_digest(data)
        test_data.append((data, hash_hex))
        if (i + 1) % 10 == 0:
            print(f"  Generated {i + 1}/{num_blobs} blobs...")
    
    summary = BenchmarkSummary(
        server=server,
        total_blobs=num_blobs,
        blob_size=blob_size,
        concurrent=concurrent
    )
    
    # Upload phase
    print(f"\nUploading {num_blobs} blobs with concurrency {concurrent}...")
    with ThreadPoolExecutor(max_workers=concurrent) as executor:
        futures = {
            executor.submit(upload_blob_batch, cas_stub, data, hash_hex): (data, hash_hex)
            for data, hash_hex in test_data
        }
        
        completed = 0
        for future in as_completed(futures):
            result = future.result()
            summary.results.append(result)
            completed += 1
            if completed % 10 == 0 or completed == num_blobs:
                print(f"  Uploaded {completed}/{num_blobs} - Success: {summary.success_count}, Failed: {summary.fail_count}")
    
    # Download phase
    print(f"\nDownloading {num_blobs} blobs with concurrency {concurrent}...")
    with ThreadPoolExecutor(max_workers=concurrent) as executor:
        futures = {
            executor.submit(download_blob_batch, cas_stub, hash_hex, blob_size): hash_hex
            for _, hash_hex in test_data
        }
        
        completed = 0
        for future in as_completed(futures):
            result = future.result()
            summary.results.append(result)
            completed += 1
            if completed % 10 == 0 or completed == num_blobs:
                upload_success = sum(1 for r in summary.results if r.operation == 'upload' and r.success)
                download_success = sum(1 for r in summary.results if r.operation == 'download' and r.success)
                print(f"  Downloaded {completed}/{num_blobs} - Uploads OK: {upload_success}, Downloads OK: {download_success}")
    
    channel.close()
    return summary


def main():
    parser = argparse.ArgumentParser(description='CAS Load Test for RBE Benchmarking')
    parser.add_argument('--server', default='localhost:9092', help='gRPC server address')
    parser.add_argument('--blobs', type=int, default=100, help='Number of blobs to test')
    parser.add_argument('--size', type=int, default=1048576, help='Blob size in bytes (default 1MB)')
    parser.add_argument('--concurrent', type=int, default=10, help='Concurrent operations')

    parser.add_argument('--output', help='Output JSON file for results')
    
    args = parser.parse_args()
    
    # Run benchmark
    summary = run_load_test(
        server=args.server,
        num_blobs=args.blobs,
        blob_size=args.size,
        concurrent=args.concurrent
    )
    
    # Print summary
    summary.print_summary()
    
    # Export to JSON if requested
    if args.output:
        import json
        result_dict = {
            'server': summary.server,
            'total_blobs': summary.total_blobs,
            'blob_size': summary.blob_size,
            'concurrent': summary.concurrent,
            'success_count': summary.success_count,
            'fail_count': summary.fail_count,
            'total_bytes': summary.total_bytes,
            'upload_latencies': {
                'min': min(summary.upload_latencies) if summary.upload_latencies else 0,
                'max': max(summary.upload_latencies) if summary.upload_latencies else 0,
                'mean': statistics.mean(summary.upload_latencies) if summary.upload_latencies else 0,
                'p50': statistics.median(summary.upload_latencies) if summary.upload_latencies else 0,
                'p95': summary._percentile(summary.upload_latencies, 95) if summary.upload_latencies else 0,
                'p99': summary._percentile(summary.upload_latencies, 99) if summary.upload_latencies else 0,
            },
            'download_latencies': {
                'min': min(summary.download_latencies) if summary.download_latencies else 0,
                'max': max(summary.download_latencies) if summary.download_latencies else 0,
                'mean': statistics.mean(summary.download_latencies) if summary.download_latencies else 0,
                'p50': statistics.median(summary.download_latencies) if summary.download_latencies else 0,
                'p95': summary._percentile(summary.download_latencies, 95) if summary.download_latencies else 0,
                'p99': summary._percentile(summary.download_latencies, 99) if summary.download_latencies else 0,
            }
        }
        with open(args.output, 'w') as f:
            json.dump(result_dict, f, indent=2)
        print(f"\nResults exported to: {args.output}")


if __name__ == '__main__':
    main()
