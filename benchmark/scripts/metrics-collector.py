#!/usr/bin/env python3
"""
Metrics Collector for RBE Benchmarking
Collects container metrics from Docker and Prometheus during benchmark runs
"""

import argparse
import json
import subprocess
import sys
import time
from dataclasses import dataclass, field, asdict
from typing import Dict, List, Optional
from datetime import datetime


@dataclass
class ContainerMetrics:
    """Container resource metrics snapshot"""
    timestamp: float
    container_name: str
    cpu_percent: float
    memory_usage_mb: float
    memory_limit_mb: float
    memory_percent: float
    net_io_read_mb: float
    net_io_write_mb: float
    block_io_read_mb: float
    block_io_write_mb: float
    pids: int


@dataclass
class BenchmarkMetrics:
    """Complete benchmark metrics collection"""
    benchmark_name: str
    start_time: str
    end_time: Optional[str] = None
    container_snapshots: List[ContainerMetrics] = field(default_factory=list)
    
    def get_memory_stats(self, container_name: str) -> Dict:
        """Get memory statistics for a specific container"""
        snapshots = [s for s in self.container_snapshots if s.container_name == container_name]
        if not snapshots:
            return {}
        
        memory_values = [s.memory_usage_mb for s in snapshots]
        return {
            'container': container_name,
            'min_mb': min(memory_values),
            'max_mb': max(memory_values),
            'avg_mb': sum(memory_values) / len(memory_values),
            'samples': len(memory_values)
        }
    
    def get_cpu_stats(self, container_name: str) -> Dict:
        """Get CPU statistics for a specific container"""
        snapshots = [s for s in self.container_snapshots if s.container_name == container_name]
        if not snapshots:
            return {}
        
        cpu_values = [s.cpu_percent for s in snapshots]
        return {
            'container': container_name,
            'min_percent': min(cpu_values),
            'max_percent': max(cpu_values),
            'avg_percent': sum(cpu_values) / len(cpu_values),
            'samples': len(cpu_values)
        }


class DockerMetricsCollector:
    """Collect metrics using Docker CLI"""
    
    def __init__(self):
        self._check_docker()
    
    def _check_docker(self):
        """Verify Docker is available"""
        try:
            subprocess.run(['docker', 'version'], capture_output=True, check=True)
        except (subprocess.CalledProcessError, FileNotFoundError):
            raise RuntimeError("Docker is not available. Please install Docker.")
    
    def get_container_stats(self, container_name: str) -> Optional[ContainerMetrics]:
        """Get stats for a single container"""
        try:
            result = subprocess.run(
                ['docker', 'stats', container_name, '--no-stream', '--format',
                 '{{.CPUPerc}}|{{.MemUsage}}|{{.MemPerc}}|{{.NetIO}}|{{.BlockIO}}|{{.PIDs}}'],
                capture_output=True,
                text=True,
                timeout=10
            )
            
            if result.returncode != 0:
                return None
            
            output = result.stdout.strip()
            if not output:
                return None
            
            # Parse: 0.15%|50MiB / 512MiB|9.77%|1.2MB / 800kB|0B / 0B|15
            parts = output.split('|')
            if len(parts) < 6:
                return None
            
            cpu_percent = float(parts[0].replace('%', ''))
            
            # Memory: "50MiB / 512MiB"
            mem_parts = parts[1].split('/')
            memory_usage = self._parse_size(mem_parts[0].strip())
            memory_limit = self._parse_size(mem_parts[1].strip())
            memory_percent = float(parts[2].replace('%', ''))
            
            # Network I/O: "1.2MB / 800kB"
            net_parts = parts[3].split('/')
            net_read = self._parse_size(net_parts[0].strip())
            net_write = self._parse_size(net_parts[1].strip())
            
            # Block I/O: "0B / 0B"
            block_parts = parts[4].split('/')
            block_read = self._parse_size(block_parts[0].strip())
            block_write = self._parse_size(block_parts[1].strip())
            
            pids = int(parts[5])
            
            return ContainerMetrics(
                timestamp=time.time(),
                container_name=container_name,
                cpu_percent=cpu_percent,
                memory_usage_mb=memory_usage,
                memory_limit_mb=memory_limit,
                memory_percent=memory_percent,
                net_io_read_mb=net_read,
                net_io_write_mb=net_write,
                block_io_read_mb=block_read,
                block_io_write_mb=block_write,
                pids=pids
            )
        
        except Exception as e:
            print(f"Error collecting metrics for {container_name}: {e}", file=sys.stderr)
            return None
    
    def _parse_size(self, size_str: str) -> float:
        """Parse size string to MB"""
        size_str = size_str.strip().upper()
        if size_str == '0B':
            return 0.0
        
        units = {
            'B': 1 / (1024 * 1024),
            'KB': 1 / 1024,
            'KIB': 1 / 1024,
            'MB': 1,
            'MIB': 1,
            'GB': 1024,
            'GIB': 1024,
            'TB': 1024 * 1024,
            'TIB': 1024 * 1024,
        }
        
        for unit, multiplier in units.items():
            if size_str.endswith(unit):
                try:
                    value = float(size_str[:-len(unit)])
                    return value * multiplier
                except ValueError:
                    return 0.0
        
        # Try to parse as plain number
        try:
            return float(size_str) / (1024 * 1024)  # Assume bytes
        except ValueError:
            return 0.0
    
    def list_containers(self, prefix: str = '') -> List[str]:
        """List running containers with optional prefix filter"""
        try:
            result = subprocess.run(
                ['docker', 'ps', '--format', '{{.Names}}'],
                capture_output=True,
                text=True,
                check=True
            )
            containers = result.stdout.strip().split('\n')
            if prefix:
                containers = [c for c in containers if c.startswith(prefix)]
            return [c for c in containers if c]
        except subprocess.CalledProcessError:
            return []


def collect_metrics(
    duration_seconds: int,
    interval_seconds: int,
    containers: List[str],
    benchmark_name: str
) -> BenchmarkMetrics:
    """Collect metrics for specified duration"""
    
    collector = DockerMetricsCollector()
    metrics = BenchmarkMetrics(
        benchmark_name=benchmark_name,
        start_time=datetime.now().isoformat()
    )
    
    print(f"Collecting metrics for {duration_seconds}s (interval: {interval_seconds}s)")
    print(f"Monitoring containers: {', '.join(containers)}")
    print("-" * 60)
    
    start_time = time.time()
    iterations = 0
    
    try:
        while time.time() - start_time < duration_seconds:
            iteration_start = time.time()
            
            for container in containers:
                snapshot = collector.get_container_stats(container)
                if snapshot:
                    metrics.container_snapshots.append(snapshot)
                    # Print current status
                    print(f"[{iterations:4d}] {container:30s} | "
                          f"CPU: {snapshot.cpu_percent:6.2f}% | "
                          f"Mem: {snapshot.memory_usage_mb:8.1f}MB ({snapshot.memory_percent:5.2f}%)")
            
            iterations += 1
            
            # Sleep until next interval
            elapsed = time.time() - iteration_start
            sleep_time = max(0, interval_seconds - elapsed)
            if sleep_time > 0:
                time.sleep(sleep_time)
    
    except KeyboardInterrupt:
        print("\nMetrics collection interrupted by user")
    
    metrics.end_time = datetime.now().isoformat()
    return metrics


def print_summary(metrics: BenchmarkMetrics):
    """Print summary statistics"""
    print("\n" + "=" * 60)
    print(f"METRICS SUMMARY: {metrics.benchmark_name}")
    print("=" * 60)
    print(f"Duration: {metrics.start_time} to {metrics.end_time}")
    print(f"Total snapshots: {len(metrics.container_snapshots)}")
    
    # Get unique container names
    containers = set(s.container_name for s in metrics.container_snapshots)
    
    print("\nMEMORY STATISTICS:")
    print("-" * 60)
    print(f"{'Container':<30} {'Min (MB)':<12} {'Max (MB)':<12} {'Avg (MB)':<12}")
    print("-" * 60)
    for container in sorted(containers):
        stats = metrics.get_memory_stats(container)
        if stats:
            print(f"{container:<30} {stats['min_mb']:<12.1f} {stats['max_mb']:<12.1f} {stats['avg_mb']:<12.1f}")
    
    print("\nCPU STATISTICS:")
    print("-" * 60)
    print(f"{'Container':<30} {'Min (%)':<12} {'Max (%)':<12} {'Avg (%)':<12}")
    print("-" * 60)
    for container in sorted(containers):
        stats = metrics.get_cpu_stats(container)
        if stats:
            print(f"{container:<30} {stats['min_percent']:<12.2f} {stats['max_percent']:<12.2f} {stats['avg_percent']:<12.2f}")
    
    print("=" * 60)


def main():
    parser = argparse.ArgumentParser(description='Collect container metrics during benchmarking')
    parser.add_argument('--duration', type=int, default=300, help='Collection duration in seconds')
    parser.add_argument('--interval', type=int, default=5, help='Sampling interval in seconds')
    parser.add_argument('--containers', nargs='+', default=[],
                       help='Container names to monitor (default: auto-detect)')
    parser.add_argument('--prefix', default='', help='Container name prefix filter')
    parser.add_argument('--benchmark', default='rbe-benchmark', help='Benchmark name')
    parser.add_argument('--output', required=True, help='Output JSON file')
    
    args = parser.parse_args()
    
    # Auto-detect containers if not specified
    containers = args.containers
    if not containers:
        collector = DockerMetricsCollector()
        containers = collector.list_containers(args.prefix)
        if not containers:
            print("No containers found to monitor", file=sys.stderr)
            sys.exit(1)
    
    # Collect metrics
    metrics = collect_metrics(
        duration_seconds=args.duration,
        interval_seconds=args.interval,
        containers=containers,
        benchmark_name=args.benchmark
    )
    
    # Print summary
    print_summary(metrics)
    
    # Export to JSON
    output_data = {
        'benchmark_name': metrics.benchmark_name,
        'start_time': metrics.start_time,
        'end_time': metrics.end_time,
        'containers': list(set(s.container_name for s in metrics.container_snapshots)),
        'snapshots': [asdict(s) for s in metrics.container_snapshots],
        'summary': {
            container: {
                'memory': metrics.get_memory_stats(container),
                'cpu': metrics.get_cpu_stats(container)
            }
            for container in set(s.container_name for s in metrics.container_snapshots)
        }
    }
    
    with open(args.output, 'w') as f:
        json.dump(output_data, f, indent=2)
    
    print(f"\nMetrics exported to: {args.output}")


if __name__ == '__main__':
    main()
