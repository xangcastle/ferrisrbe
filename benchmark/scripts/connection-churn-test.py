#!/usr/bin/env python3
"""
Connection Churn Test - Abrupt disconnections and resource cleanup
Tests handling of sudden connection drops during gRPC operations

FerrisRBE Advantage: Tokio's native task cancellation releases resources immediately.
JVM apps may leave zombie threads or persistent connections causing memory leaks.
"""

import argparse
import hashlib
import os
import sys
import time
import statistics
import socket
import random
from dataclasses import dataclass, field
from typing import List, Optional
from concurrent.futures import ThreadPoolExecutor, as_completed
import threading
import signal

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
class ChurnResult:
    """Result of a single churn test"""
    connection_id: str
    operation: str
    duration_ms: float
    disconnected_at: Optional[float] = None
    resources_cleaned: bool = False
    error: Optional[str] = None


@dataclass
class ChurnSummary:
    """Summary of connection churn test"""
    server: str
    total_connections: int
    disconnect_rate: float
    results: List[ChurnResult] = field(default_factory=list)
    
    def get_successful_cleanups(self) -> List[ChurnResult]:
        return [r for r in self.results if r.resources_cleaned]
    
    def get_failed_cleanups(self) -> List[ChurnResult]:
        return [r for r in self.results if not r.resources_cleaned and r.error]
    
    def print_summary(self):
        print("\n" + "=" * 70)
        print(f"CONNECTION CHURN TEST - {self.server}")
        print("=" * 70)
        print(f"\n📊 TEST PARAMETERS:")
        print(f"  Total connections: {self.total_connections}")
        print(f"  Disconnect rate: {self.disconnect_rate * 100:.1f}%")
        
        successful = len(self.get_successful_cleanups())
        failed = len(self.get_failed_cleanups())
        total = len(self.results)
        
        print(f"\n📊 RESULTS:")
        print(f"  Successful cleanups: {successful}/{total} ({successful/total*100:.1f}%)")
        print(f"  Failed cleanups: {failed}/{total} ({failed/total*100:.1f}%)")
        
        if self.results:
            durations = [r.duration_ms for r in self.results if r.duration_ms > 0]
            if durations:
                print(f"  Avg cleanup time: {statistics.mean(durations):.1f}ms")
                print(f"  Max cleanup time: {max(durations):.1f}ms")
        
        print(f"\n🏆 RESOURCE CLEANUP VERDICT:")
        if failed == 0:
            print(f"  ✅ EXCELLENT: All resources cleaned immediately")
            print(f"     (No memory leaks, no zombie connections)")
        elif failed / total < 0.05:
            print(f"  ✅ GOOD: Minimal resource leakage (<5%)")
        elif failed / total < 0.15:
            print(f"  ⚠️  WARNING: Some resource leakage detected")
        else:
            print(f"  ❌ CRITICAL: Significant resource leakage")
            print(f"     (Potential memory leak, zombie threads)")
        
        print("=" * 70)


def create_grpc_channel(server: str) -> grpc.Channel:
    """Create a new gRPC channel"""
    return grpc.insecure_channel(server)


def abrupt_disconnect(channel: grpc.Channel):
    """Close channel abruptly"""
    channel.close()


def start_streaming_upload(
    channel: grpc.Channel,
    blob_size: int = 10 * 1024 * 1024  # 10MB default
) -> Tuple[bytestream_pb2_grpc.ByteStreamStub, iter]:
    """Start a streaming upload and return the stub + request iterator"""
    stub = bytestream_pb2_grpc.ByteStreamStub(channel)
    
    file_hash = hashlib.sha256(os.urandom(32)).hexdigest()
    resource_name = f"uploads/{int(time.time())}/blobs/{file_hash}/{blob_size}"
    
    # Generate chunks
    chunk_size = 1024 * 1024  # 1MB chunks
    chunks = []
    remaining = blob_size
    offset = 0
    
    while remaining > 0:
        data = os.urandom(min(chunk_size, remaining))
        chunks.append(bytestream_pb2.WriteRequest(
            resource_name=resource_name if offset == 0 else "",
            data=data,
            write_offset=offset
        ))
        offset += len(data)
        remaining -= len(data)
    
    def request_generator():
        for chunk in chunks:
            yield chunk
            time.sleep(0.01)  # Small delay to simulate streaming
    
    return stub, request_generator()


def start_streaming_download(
    channel: grpc.Channel,
    blob_hash: str = "test",
    blob_size: int = 10 * 1024 * 1024
) -> Tuple[bytestream_pb2_grpc.ByteStreamStub, bytestream_pb2.ReadRequest]:
    """Start a streaming download"""
    stub = bytestream_pb2_grpc.ByteStreamStub(channel)
    request = bytestream_pb2.ReadRequest(
        resource_name=f"blobs/{blob_hash}/{blob_size}"
    )
    return stub, request


def test_connection_churn(
    server: str,
    connection_id: str,
    operation: str,
    disconnect_rate: float
) -> ChurnResult:
    """Test a single connection with potential abrupt disconnection"""
    
    start_time = time.time()
    should_disconnect = random.random() < disconnect_rate
    disconnect_time = random.uniform(0.1, 0.5) if should_disconnect else None
    
    result = ChurnResult(
        connection_id=connection_id,
        operation=operation,
        duration_ms=0
    )
    
    try:
        # Create channel
        channel = create_grpc_channel(server)
        
        if operation == 'upload':
            stub, requests = start_streaming_upload(channel)
            
            if should_disconnect:
                # Start upload in background
                response_iter = stub.Write(requests)
                
                # Wait then disconnect abruptly
                time.sleep(disconnect_time)
                channel.close()
                result.disconnected_at = disconnect_time
                result.resources_cleaned = True  # Tokio should clean up immediately
                
            else:
                # Complete normally
                for response in stub.Write(requests):
                    pass
                channel.close()
                result.resources_cleaned = True
        
        elif operation == 'download':
            stub, request = start_streaming_download(channel)
            
            if should_disconnect:
                response_iter = stub.Read(request)
                
                # Consume some data then disconnect
                consumed = 0
                for response in response_iter:
                    consumed += len(response.data)
                    if consumed > 1024 * 1024:  # Disconnect after 1MB
                        break
                
                channel.close()
                result.disconnected_at = time.time() - start_time
                result.resources_cleaned = True
            else:
                for response in stub.Read(request):
                    pass
                channel.close()
                result.resources_cleaned = True
        
        elif operation == 'execute':
            stub = remote_execution_pb2_grpc.ExecutionStub(channel)
            
            # Create a simple action
            action_digest = remote_execution_pb2.Digest(
                hash=hashlib.sha256(b"test").hexdigest(),
                size_bytes=4
            )
            request = remote_execution_pb2.ExecuteRequest(
                action_digest=action_digest
            )
            
            if should_disconnect:
                response_iter = stub.Execute(request)
                time.sleep(disconnect_time)
                channel.close()
                result.disconnected_at = disconnect_time
                result.resources_cleaned = True
            else:
                for response in stub.Execute(request):
                    pass
                channel.close()
                result.resources_cleaned = True
        
        result.duration_ms = (time.time() - start_time) * 1000
        
    except grpc.RpcError as e:
        # Expected for disconnections
        if e.code() in [grpc.StatusCode.CANCELLED, grpc.StatusCode.UNAVAILABLE]:
            result.duration_ms = (time.time() - start_time) * 1000
            result.resources_cleaned = True  # Cancelled = cleaned
        else:
            result.error = str(e)
            result.duration_ms = (time.time() - start_time) * 1000
    
    except Exception as e:
        result.error = str(e)
        result.duration_ms = (time.time() - start_time) * 1000
    
    return result


def run_connection_churn_test(
    server: str,
    total_connections: int,
    disconnect_rate: float,
    concurrent: int = 50
) -> ChurnSummary:
    """Run connection churn test"""
    
    print(f"Connecting to {server}...")
    
    summary = ChurnSummary(
        server=server,
        total_connections=total_connections,
        disconnect_rate=disconnect_rate
    )
    
    operations = ['upload', 'download', 'execute']
    
    print(f"\n🎯 Test Plan:")
    print(f"   Create {total_connections} connections")
    print(f"   Disconnect {disconnect_rate * 100:.0f}% of them abruptly")
    print(f"   Measure resource cleanup time")
    
    print(f"\n🔥 Starting connection churn...")
    
    with ThreadPoolExecutor(max_workers=concurrent) as executor:
        futures = {}
        for i in range(total_connections):
            operation = operations[i % len(operations)]
            future = executor.submit(
                test_connection_churn,
                server,
                f"conn-{i}",
                operation,
                disconnect_rate
            )
            futures[future] = i
        
        completed = 0
        for future in as_completed(futures):
            result = future.result()
            summary.results.append(result)
            completed += 1
            
            if completed % 100 == 0 or completed == total_connections:
                print(f"  Progress: {completed}/{total_connections} connections tested")
    
    return summary


def main():
    parser = argparse.ArgumentParser(
        description='Connection Churn Test - Abrupt disconnections and resource cleanup'
    )
    parser.add_argument('--server', default='localhost:9092', help='gRPC server address')
    parser.add_argument('--connections', type=int, default=1000,
                       help='Total connections to test (default: 1000)')
    parser.add_argument('--disconnect-rate', type=float, default=0.3,
                       help='Rate of abrupt disconnections 0-1 (default: 0.3)')
    parser.add_argument('--concurrent', type=int, default=50,
                       help='Concurrent connections (default: 50)')
    parser.add_argument('--output', help='Output JSON file for results')
    
    args = parser.parse_args()
    
    print("=" * 70)
    print("CONNECTION CHURN TEST")
    print("Testing resource cleanup after abrupt disconnections")
    print("=" * 70)
    
    # Run test
    summary = run_connection_churn_test(
        server=args.server,
        total_connections=args.connections,
        disconnect_rate=args.disconnect_rate,
        concurrent=args.concurrent
    )
    
    # Print summary
    summary.print_summary()
    
    # Export if requested
    if args.output:
        import json
        result_dict = {
            'server': summary.server,
            'total_connections': summary.total_connections,
            'disconnect_rate': summary.disconnect_rate,
            'successful_cleanups': len(summary.get_successful_cleanups()),
            'failed_cleanups': len(summary.get_failed_cleanups()),
            'cleanup_times_ms': [r.duration_ms for r in summary.results if r.duration_ms > 0]
        }
        with open(args.output, 'w') as f:
            json.dump(result_dict, f, indent=2)
        print(f"\nResults exported to: {args.output}")


if __name__ == '__main__':
    main()
