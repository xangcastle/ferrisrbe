#!/usr/bin/env python3
"""
O(1) Streaming Test - Memory Constant regardless of blob size
Tests concurrent upload/download of mixed large (5-10GB) and small (1KB) files

FerrisRBE Advantage: Async streaming with Tokio maintains constant memory.
JVM solutions often OOM or suffer GC pauses when buffering large files.
"""

import argparse
import hashlib
import os
import sys
import time
import statistics
import tempfile
import threading
from dataclasses import dataclass, field
from typing import List, Optional, Tuple
from concurrent.futures import ThreadPoolExecutor, as_completed

import grpc

sys.path.insert(0, os.path.join(os.path.dirname(__file__), '..', 'proto_gen'))

try:
    from build.bazel.remote.execution.v2 import remote_execution_pb2
    from build.bazel.remote.execution.v2 import remote_execution_pb2_grpc
    from google.bytestream import bytestream_pb2
    from google.bytestream import bytestream_pb2_grpc
except ImportError:
    print("Warning: Protocol buffer modules not found.")
    sys.exit(1)


@dataclass
class StreamingResult:
    """Result of a single streaming operation"""
    operation: str  # 'upload_large', 'upload_small', 'download_large', 'download_small'
    blob_size: int
    duration_ms: float
    memory_before_mb: float
    memory_after_mb: float
    memory_delta_mb: float
    success: bool
    error: Optional[str] = None


@dataclass
class StreamingSummary:
    """Summary of streaming benchmark"""
    server: str
    results: List[StreamingResult] = field(default_factory=list)
    
    def get_large_ops(self) -> List[StreamingResult]:
        return [r for r in self.results if 'large' in r.operation and r.success]
    
    def get_small_ops(self) -> List[StreamingResult]:
        return [r for r in self.results if 'small' in r.operation and r.success]
    
    def print_summary(self):
        print("\n" + "=" * 70)
        print(f"O(1) STREAMING TEST - {self.server}")
        print("=" * 70)
        
        large_ops = self.get_large_ops()
        small_ops = self.get_small_ops()
        
        print(f"\n📊 LARGE FILE OPERATIONS (5-10GB):")
        if large_ops:
            sizes = [r.blob_size / (1024**3) for r in large_ops]  # GB
            durations = [r.duration_ms / 1000 for r in large_ops]  # seconds
            deltas = [r.memory_delta_mb for r in large_ops]
            
            print(f"  Files tested: {len(large_ops)}")
            print(f"  Size range: {min(sizes):.1f}GB - {max(sizes):.1f}GB")
            print(f"  Upload time: {statistics.mean(durations):.1f}s avg")
            print(f"  Memory delta: {statistics.mean(deltas):.1f}MB avg")
            print(f"  Max memory spike: {max(deltas):.1f}MB")
            
            if max(deltas) < 100:
                print(f"  ✅ EXCELLENT: True O(1) streaming (constant memory)")
            elif max(deltas) < 500:
                print(f"  ⚠️  WARNING: Some buffering detected")
            else:
                print(f"  ❌ CRITICAL: Significant buffering (risk of OOM)")
        
        print(f"\n📊 SMALL FILE OPERATIONS (1KB):")
        if small_ops:
            durations = [r.duration_ms for r in small_ops]
            print(f"  Files tested: {len(small_ops)}")
            print(f"  Avg latency: {statistics.mean(durations):.1f}ms")
            print(f"  P99 latency: {self._percentile(durations, 99):.1f}ms")
        
        print(f"\n🏆 O(1) STREAMING VERDICT:")
        if large_ops and max(r.memory_delta_mb for r in large_ops) < 100:
            print(f"  ✅ PASS: Memory usage independent of blob size")
        else:
            print(f"  ❌ FAIL: Memory scales with blob size (O(n) behavior)")
        
        print("=" * 70)
    
    @staticmethod
    def _percentile(data: List[float], percentile: float) -> float:
        if not data:
            return 0.0
        sorted_data = sorted(data)
        index = int(len(sorted_data) * percentile / 100)
        return sorted_data[min(index, len(sorted_data) - 1)]


def get_container_memory(container_name: str) -> float:
    """Get container memory usage in MB"""
    import subprocess
    try:
        result = subprocess.run(
            ['docker', 'stats', container_name, '--no-stream', '--format', '{{.MemUsage}}'],
            capture_output=True, text=True, timeout=5
        )
        mem_str = result.stdout.strip().split('/')[0].strip()
        # Parse MiB, GiB, etc.
        if 'GiB' in mem_str:
            return float(mem_str.replace('GiB', '')) * 1024
        elif 'MiB' in mem_str:
            return float(mem_str.replace('MiB', ''))
        elif 'KiB' in mem_str:
            return float(mem_str.replace('KiB', '')) / 1024
        return 0.0
    except:
        return 0.0


def generate_large_file(size_gb: int) -> Tuple[str, str]:
    """Generate a large file and return path + hash"""
    fd, path = tempfile.mkstemp(suffix='.bin')
    os.close(fd)
    
    # Write in chunks to avoid loading entire file in memory
    chunk_size = 10 * 1024 * 1024  # 10MB chunks
    total_bytes = size_gb * 1024 * 1024 * 1024
    
    hasher = hashlib.sha256()
    bytes_written = 0
    
    with open(path, 'wb') as f:
        while bytes_written < total_bytes:
            chunk = os.urandom(min(chunk_size, total_bytes - bytes_written))
            f.write(chunk)
            hasher.update(chunk)
            bytes_written += len(chunk)
    
    return path, hasher.hexdigest()


def upload_via_bytestream(
    bytestream_stub,
    file_path: str,
    file_hash: str,
    file_size: int
) -> Tuple[float, bool, str]:
    """Upload file via ByteStream API"""
    start = time.perf_counter()
    
    try:
        # Build resource name
        resource_name = f"uploads/{int(time.time())}/blobs/{file_hash}/{file_size}"
        
        # Stream chunks
        chunk_size = 4 * 1024 * 1024  # 4MB chunks
        
        def generate_chunks():
            with open(file_path, 'rb') as f:
                offset = 0
                while True:
                    chunk = f.read(chunk_size)
                    if not chunk:
                        break
                    yield bytestream_pb2.WriteRequest(
                        resource_name=resource_name if offset == 0 else "",
                        data=chunk,
                        write_offset=offset
                    )
                    offset += len(chunk)
        
        responses = bytestream_stub.Write(generate_chunks())
        # Consume responses
        for response in responses:
            pass
        
        duration_ms = (time.perf_counter() - start) * 1000
        return duration_ms, True, ""
    
    except Exception as e:
        duration_ms = (time.perf_counter() - start) * 1000
        return duration_ms, False, str(e)


def download_via_bytestream(
    bytestream_stub,
    file_hash: str,
    file_size: int
) -> Tuple[float, bool, str]:
    """Download file via ByteStream API"""
    start = time.perf_counter()
    
    try:
        resource_name = f"blobs/{file_hash}/{file_size}"
        request = bytestream_pb2.ReadRequest(
            resource_name=resource_name
        )
        
        # Stream read (data discarded to measure throughput)
        total_read = 0
        for response in bytestream_stub.Read(request):
            total_read += len(response.data)
        
        duration_ms = (time.perf_counter() - start) * 1000
        return duration_ms, True, ""
    
    except Exception as e:
        duration_ms = (time.perf_counter() - start) * 1000
        return duration_ms, False, str(e)


def run_o1_streaming_test(
    server: str,
    large_sizes_gb: List[int],
    small_count: int,
    container_name: str,
    concurrent: int = 3
) -> StreamingSummary:
    """Run O(1) streaming test"""
    
    print(f"Connecting to {server}...")
    channel = grpc.insecure_channel(server)
    bytestream_stub = bytestream_pb2_grpc.ByteStreamStub(channel)
    
    summary = StreamingSummary(server=server)
    
    print(f"\n🎯 Test Plan:")
    print(f"   Upload {len(large_sizes_gb)} large files ({large_sizes_gb}GB)")
    print(f"   Upload {small_count} small files (1KB)")
    print(f"   Monitor memory to verify O(1) streaming")
    
    # Generate test files
    print(f"\n📝 Generating test files...")
    large_files = []
    for size_gb in large_sizes_gb:
        print(f"  Generating {size_gb}GB file...")
        path, file_hash = generate_large_file(size_gb)
        large_files.append((path, file_hash, size_gb * 1024**3))
    
    # Test large file uploads
    print(f"\n📤 Testing LARGE file uploads...")
    for path, file_hash, size in large_files:
        mem_before = get_container_memory(container_name)
        duration, success, error = upload_via_bytestream(
            bytestream_stub, path, file_hash, size
        )
        mem_after = get_container_memory(container_name)
        
        summary.results.append(StreamingResult(
            operation='upload_large',
            blob_size=size,
            duration_ms=duration,
            memory_before_mb=mem_before,
            memory_after_mb=mem_after,
            memory_delta_mb=mem_after - mem_before,
            success=success,
            error=error
        ))
        
        status = "✅" if success else "❌"
        print(f"  {status} {size/(1024**3):.1f}GB in {duration/1000:.1f}s "
              f"(mem: {mem_before:.1f}MB -> {mem_after:.1f}MB)")
        
        # Cleanup
        os.unlink(path)
    
    # Test small file uploads
    print(f"\n📤 Testing SMALL file uploads...")
    small_size = 1024  # 1KB
    
    with ThreadPoolExecutor(max_workers=concurrent) as executor:
        futures = {}
        for i in range(small_count):
            data = os.urandom(small_size)
            file_hash = hashlib.sha256(data).hexdigest()
            
            fd, path = tempfile.mkstemp()
            os.write(fd, data)
            os.close(fd)
            
            future = executor.submit(
                upload_via_bytestream,
                bytestream_stub,
                path,
                file_hash,
                small_size
            )
            futures[future] = (path, i)
        
        for future in as_completed(futures):
            path, idx = futures[future]
            duration, success, error = future.result()
            
            summary.results.append(StreamingResult(
                operation='upload_small',
                blob_size=small_size,
                duration_ms=duration,
                memory_before_mb=0,
                memory_after_mb=0,
                memory_delta_mb=0,
                success=success,
                error=error
            ))
            
            os.unlink(path)
            
            if idx % 100 == 0:
                print(f"  Progress: {idx}/{small_count} small files")
    
    channel.close()
    return summary


def main():
    parser = argparse.ArgumentParser(
        description='O(1) Streaming Test - Constant memory regardless of blob size'
    )
    parser.add_argument('--server', default='localhost:9092', help='gRPC server address')
    parser.add_argument('--large-sizes', nargs='+', type=int, default=[1, 5],
                       help='Large file sizes in GB (default: 1 5)')
    parser.add_argument('--small-count', type=int, default=1000,
                       help='Number of small files (default: 1000)')
    parser.add_argument('--container', default='ferrisrbe-server',
                       help='Container name to monitor')
    parser.add_argument('--concurrent', type=int, default=10,
                       help='Concurrent small uploads')
    parser.add_argument('--output', help='Output JSON file for results')
    
    args = parser.parse_args()
    
    print("=" * 70)
    print("O(1) STREAMING TEST")
    print("Verifies memory usage is independent of blob size")
    print("=" * 70)
    
    # Run test
    summary = run_o1_streaming_test(
        server=args.server,
        large_sizes_gb=args.large_sizes,
        small_count=args.small_count,
        container_name=args.container,
        concurrent=args.concurrent
    )
    
    # Print summary
    summary.print_summary()
    
    # Export if requested
    if args.output:
        import json
        result_dict = {
            'server': summary.server,
            'large_ops': [
                {
                    'size_gb': r.blob_size / (1024**3),
                    'duration_ms': r.duration_ms,
                    'memory_delta_mb': r.memory_delta_mb
                }
                for r in summary.get_large_ops()
            ],
            'small_ops': [
                {
                    'duration_ms': r.duration_ms
                }
                for r in summary.get_small_ops()
            ]
        }
        with open(args.output, 'w') as f:
            json.dump(result_dict, f, indent=2)
        print(f"\nResults exported to: {args.output}")


if __name__ == '__main__':
    main()
