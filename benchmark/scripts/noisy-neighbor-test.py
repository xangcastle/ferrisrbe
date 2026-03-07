#!/usr/bin/env python3
"""
Noisy Neighbor Test for RBE Multi-Level Scheduler
Tests if fast actions get blocked behind slow actions (Head-of-Line Blocking)

This demonstrates FerrisRBE's advantage with multi-level scheduling:
- Fast actions (< 1s) go to fast queue
- Slow actions (> 10s) go to slow queue
- Fast actions should NOT wait behind slow actions

Traditional FIFO schedulers (Redis-backed) will suffer from HoL blocking.
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
from datetime import datetime

import grpc

sys.path.insert(0, os.path.join(os.path.dirname(__file__), '..', 'proto_gen'))

try:
    from build.bazel.remote.execution.v2 import remote_execution_pb2
    from build.bazel.remote.execution.v2 import remote_execution_pb2_grpc
except ImportError:
    print("Warning: Protocol buffer modules not found.")
    sys.exit(1)


@dataclass
class ActionTiming:
    """Timing information for a single action"""
    action_id: str
    action_type: str  # 'fast' or 'slow'
    submitted_at: float
    started_at: Optional[float] = None
    completed_at: Optional[float] = None
    
    @property
    def queue_time_ms(self) -> float:
        """Time spent in queue"""
        if self.started_at and self.submitted_at:
            return (self.started_at - self.submitted_at) * 1000
        return 0.0
    
    @property
    def total_time_ms(self) -> float:
        """Total time from submission to completion"""
        if self.completed_at and self.submitted_at:
            return (self.completed_at - self.submitted_at) * 1000
        return 0.0


@dataclass
class NoisyNeighborSummary:
    """Summary of noisy neighbor test"""
    server: str
    slow_actions: int
    fast_actions: int
    timings: List[ActionTiming] = field(default_factory=list)
    
    def get_fast_timings(self) -> List[ActionTiming]:
        return [t for t in self.timings if t.action_type == 'fast']
    
    def get_slow_timings(self) -> List[ActionTiming]:
        return [t for t in self.timings if t.action_type == 'slow']
    
    def print_summary(self):
        print("\n" + "=" * 70)
        print(f"NOISY NEIGHBOR TEST - {self.server}")
        print("=" * 70)
        print(f"Slow actions (10s): {self.slow_actions}")
        print(f"Fast actions (0.1s): {self.fast_actions}")
        
        fast_timings = self.get_fast_timings()
        slow_timings = self.get_slow_timings()
        
        if fast_timings:
            fast_queue_times = [t.queue_time_ms for t in fast_timings if t.queue_time_ms > 0]
            fast_total_times = [t.total_time_ms for t in fast_timings if t.total_time_ms > 0]
            
            print(f"\n📊 FAST ACTIONS (should be ~100ms regardless of slow actions):")
            if fast_queue_times:
                print(f"  Queue time - Min: {min(fast_queue_times):.0f}ms, "
                      f"Avg: {statistics.mean(fast_queue_times):.0f}ms, "
                      f"Max: {max(fast_queue_times):.0f}ms")
            if fast_total_times:
                print(f"  Total time - Min: {min(fast_total_times):.0f}ms, "
                      f"Avg: {statistics.mean(fast_total_times):.0f}ms, "
                      f"Max: {max(fast_total_times):.0f}ms")
                
                avg_total = statistics.mean(fast_total_times)
                if avg_total < 500:
                    print(f"  ✅ EXCELLENT: Fast actions complete quickly!")
                elif avg_total < 2000:
                    print(f"  ⚠️  WARNING: Some head-of-line blocking detected")
                else:
                    print(f"  ❌ CRITICAL: Severe head-of-line blocking!")
        
        if slow_timings:
            slow_total_times = [t.total_time_ms for t in slow_timings if t.total_time_ms > 0]
            if slow_total_times:
                print(f"\n📊 SLOW ACTIONS (baseline ~10,000ms):")
                print(f"  Total time - Min: {min(slow_total_times):.0f}ms, "
                      f"Avg: {statistics.mean(slow_total_times):.0f}ms, "
                      f"Max: {max(slow_total_times):.0f}ms")
        
        # Calculate HoL blocking metric
        if fast_timings and slow_timings:
            fast_avg = statistics.mean([t.total_time_ms for t in fast_timings if t.total_time_ms > 0])
            print(f"\n🏆 HEAD-OF-LINE BLOCKING SCORE:")
            print(f"  Fast action avg: {fast_avg:.0f}ms")
            if fast_avg < 200:
                print(f"  ✅ NO HoL BLOCKING (Multi-level scheduler working!)")
            elif fast_avg < 1000:
                print(f"  ⚠️  MINIMAL HoL BLOCKING")
            else:
                print(f"  ❌ SIGNIFICANT HoL BLOCKING (FIFO scheduler?)")
        
        print("=" * 70)


def create_sleep_action(duration_secs: int) -> tuple:
    """Create a sleep action for testing"""
    # Command that sleeps for specified duration
    command_proto = remote_execution_pb2.Command(
        arguments=["sleep", str(duration_secs)],
        output_paths=[]
    )
    command_bytes = command_proto.SerializeToString()
    command_digest = remote_execution_pb2.Digest(
        hash=hashlib.sha256(command_bytes).hexdigest(),
        size_bytes=len(command_bytes)
    )
    
    action_proto = remote_execution_pb2.Action(
        command_digest=command_digest,
        do_not_cache=True  # Don't cache these test actions
    )
    action_bytes = action_proto.SerializeToString()
    action_digest = remote_execution_pb2.Digest(
        hash=hashlib.sha256(action_bytes).hexdigest(),
        size_bytes=len(action_bytes)
    )
    
    return action_digest, action_bytes, command_digest, command_bytes


def execute_with_timing(
    execution_stub: remote_execution_pb2_grpc.ExecutionStub,
    cas_stub: remote_execution_pb2_grpc.ContentAddressableStorageStub,
    action_digest: remote_execution_pb2.Digest,
    action_bytes: bytes,
    command_digest: remote_execution_pb2.Digest,
    command_bytes: bytes,
    action_id: str,
    action_type: str
) -> ActionTiming:
    """Execute action and capture timing"""
    
    timing = ActionTiming(
        action_id=action_id,
        action_type=action_type,
        submitted_at=time.time()
    )
    
    try:
        # Upload action to CAS first
        cas_request = remote_execution_pb2.BatchUpdateBlobsRequest(
            requests=[
                remote_execution_pb2.BatchUpdateBlobsRequest.Request(
                    digest=action_digest,
                    data=action_bytes
                ),
                remote_execution_pb2.BatchUpdateBlobsRequest.Request(
                    digest=command_digest,
                    data=command_bytes
                )
            ]
        )
        cas_stub.BatchUpdateBlobs(cas_request)
        
        # Submit execution
        request = remote_execution_pb2.ExecuteRequest(
            action_digest=action_digest,
            skip_cache_lookup=True
        )
        
        # Stream responses
        timing.started_at = time.time()
        for operation in execution_stub.Execute(request):
            if operation.done:
                break
        timing.completed_at = time.time()
        
    except Exception as e:
        print(f"Error executing {action_id}: {e}")
        timing.completed_at = time.time()
    
    return timing


def run_noisy_neighbor_test(
    server: str,
    slow_count: int,
    fast_count: int,
    slow_duration: int = 10,
    fast_duration: int = 0
) -> NoisyNeighborSummary:
    """
    Run noisy neighbor test
    
    Strategy:
    1. Submit many slow actions (10s each)
    2. While slow actions are running, submit fast actions (0.1s each)
    3. Measure if fast actions get blocked behind slow ones
    """
    
    print(f"Connecting to {server}...")
    channel = grpc.insecure_channel(server)
    execution_stub = remote_execution_pb2_grpc.ExecutionStub(channel)
    cas_stub = remote_execution_pb2_grpc.ContentAddressableStorageStub(channel)
    
    summary = NoisyNeighborSummary(
        server=server,
        slow_actions=slow_count,
        fast_actions=fast_count
    )
    
    print(f"\n🎯 Test Plan:")
    print(f"   Submit {slow_count} SLOW actions ({slow_duration}s each)")
    print(f"   While those run, submit {fast_count} FAST actions ({fast_duration}s each)")
    print(f"   Measure if fast actions get stuck behind slow ones")
    
    # Create action digests
    print(f"\n📝 Creating action digests...")
    slow_actions = []
    for i in range(slow_count):
        digest, bytes_data, cmd_digest, cmd_bytes = create_sleep_action(slow_duration)
        slow_actions.append((f"slow-{i}", digest, bytes_data, cmd_digest, cmd_bytes))
    
    fast_actions = []
    for i in range(fast_count):
        # Use 0 duration for instant completion (or 0.1 for minimal work)
        duration = max(fast_duration, 0)
        digest, bytes_data, cmd_digest, cmd_bytes = create_sleep_action(duration)
        fast_actions.append((f"fast-{i}", digest, bytes_data, cmd_digest, cmd_bytes))
    
    # Submit all actions concurrently
    print(f"\n🚀 Submitting all actions concurrently...")
    all_actions = [(a[0], a[1], a[2], a[3], a[4], 'slow') for a in slow_actions] + \
                  [(a[0], a[1], a[2], a[3], a[4], 'fast') for a in fast_actions]
    
    start_time = time.time()
    
    with ThreadPoolExecutor(max_workers=slow_count + fast_count) as executor:
        futures = {
            executor.submit(
                execute_with_timing,
                execution_stub,
                cas_stub,
                action[1],  # digest
                action[2],  # bytes
                action[3],  # cmd_digest
                action[4],  # cmd_bytes
                action[0],  # action_id
                action[5]   # action_type
            ): action
            for action in all_actions
        }
        
        completed = 0
        for future in as_completed(futures):
            timing = future.result()
            summary.timings.append(timing)
            completed += 1
            
            if timing.action_type == 'fast' and timing.completed_at:
                elapsed = (time.time() - start_time) * 1000
                print(f"  [{completed}/{len(all_actions)}] Fast action completed in "
                      f"{timing.total_time_ms:.0f}ms (wall: {elapsed:.0f}ms)")
            elif completed % 10 == 0:
                print(f"  [{completed}/{len(all_actions)}] actions completed...")
    
    channel.close()
    return summary


def main():
    parser = argparse.ArgumentParser(
        description='Noisy Neighbor Test - Tests multi-level scheduler'
    )
    parser.add_argument('--server', default='localhost:9092', help='gRPC server address')
    parser.add_argument('--slow', type=int, default=10, help='Number of slow actions')
    parser.add_argument('--fast', type=int, default=50, help='Number of fast actions')
    parser.add_argument('--slow-duration', type=int, default=10,
                       help='Duration of slow actions (seconds)')
    parser.add_argument('--fast-duration', type=int, default=0,
                       help='Duration of fast actions (seconds, 0=instant)')
    parser.add_argument('--output', help='Output JSON file for results')
    
    args = parser.parse_args()
    
    print("=" * 70)
    print("NOISY NEIGHBOR TEST")
    print("Tests if fast actions get blocked behind slow actions")
    print("=" * 70)
    
    # Run test
    summary = run_noisy_neighbor_test(
        server=args.server,
        slow_count=args.slow,
        fast_count=args.fast,
        slow_duration=args.slow_duration,
        fast_duration=args.fast_duration
    )
    
    # Print summary
    summary.print_summary()
    
    # Export if requested
    if args.output:
        import json
        result_dict = {
            'server': summary.server,
            'slow_actions': summary.slow_actions,
            'fast_actions': summary.fast_actions,
            'fast_timings': [
                {
                    'action_id': t.action_id,
                    'queue_time_ms': t.queue_time_ms,
                    'total_time_ms': t.total_time_ms
                }
                for t in summary.get_fast_timings()
            ],
            'slow_timings': [
                {
                    'action_id': t.action_id,
                    'queue_time_ms': t.queue_time_ms,
                    'total_time_ms': t.total_time_ms
                }
                for t in summary.get_slow_timings()
            ]
        }
        with open(args.output, 'w') as f:
            json.dump(result_dict, f, indent=2)
        print(f"\nResults exported to: {args.output}")


if __name__ == '__main__':
    main()
