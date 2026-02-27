#!/usr/bin/env python3
"""
Performance Regression Checker
Compares benchmark results against thresholds and fails if regressions are detected

Usage:
    python check-regression.py benchmark_data.json
"""

import json
import sys
import argparse
from typing import Dict, List, Tuple
from dataclasses import dataclass


@dataclass
class Threshold:
    """Performance threshold for a metric"""
    metric: str
    max_value: float
    unit: str


# Define regression thresholds
# These values are based on expected FerrisRBE performance
THRESHOLDS = {
    "memory_mb": Threshold("memory_mb", 20.0, "MB"),  # Idle memory should be <20MB
    "cold_start_ms": Threshold("cold_start_ms", 500.0, "ms"),  # Cold start <500ms
    "execution_p99_ms": Threshold("execution_p99_ms", 100.0, "ms"),  # P99 latency
    "cache_p99_us": Threshold("cache_p99_us", 1000.0, "μs"),  # Cache read P99
    "churn_cleanup_rate": Threshold("churn_cleanup_rate", 95.0, "%"),  # Cleanup rate
    "streaming_delta_mb": Threshold("streaming_delta_mb", 100.0, "MB"),  # O(1) streaming
}


def load_benchmark_data(filepath: str) -> Dict:
    """Load benchmark data from JSON file"""
    try:
        with open(filepath, 'r') as f:
            return json.load(f)
    except (FileNotFoundError, json.JSONDecodeError) as e:
        print(f"ERROR: Failed to load benchmark data: {e}")
        sys.exit(1)


def check_threshold(metric: str, value: float, threshold: Threshold) -> Tuple[bool, str]:
    """
    Check if a value exceeds threshold
    Returns (passed, message)
    """
    if value is None:
        return True, f"⚠️  {metric}: No data"
    
    passed = value <= threshold.max_value
    
    if passed:
        status = "✅"
        indicator = "PASS"
    else:
        status = "🚨"
        indicator = "REGRESSION"
    
    message = f"{status} {metric}: {value:.2f}{threshold.unit} (threshold: {threshold.max_value}{threshold.unit}) - {indicator}"
    
    return passed, message


def check_regressions(data: Dict) -> Tuple[bool, List[str]]:
    """
    Check all metrics for regressions
    Returns (all_passed, messages)
    """
    results = data.get("results", {})
    messages = []
    all_passed = True
    
    print("=" * 70)
    print("PERFORMANCE REGRESSION CHECK")
    print("=" * 70)
    print()
    
    for metric, threshold in THRESHOLDS.items():
        value = results.get(metric)
        
        # Handle nested values
        if value is None and "." in metric:
            parts = metric.split(".")
            value = results
            for part in parts:
                value = value.get(part) if isinstance(value, dict) else None
        
        passed, message = check_threshold(metric, value, threshold)
        messages.append(message)
        
        if not passed:
            all_passed = False
    
    return all_passed, messages


def generate_report(all_passed: bool, messages: List[str], output_file: str = None):
    """Generate and optionally save regression report"""
    report_lines = [
        "## Performance Regression Check Results\n",
        ""
    ]
    
    for message in messages:
        report_lines.append(message)
    
    report_lines.append("")
    report_lines.append("-" * 70)
    
    if all_passed:
        report_lines.append("✅ ALL CHECKS PASSED - No regressions detected")
    else:
        report_lines.append("🚨 REGRESSIONS DETECTED - Please optimize before merging")
    
    report = "\n".join(report_lines)
    
    print()
    print(report)
    
    if output_file:
        with open(output_file, 'w') as f:
            f.write(report)
        print(f"\nReport saved to: {output_file}")
    
    return report


def compare_with_baseline(current: Dict, baseline_file: str) -> Tuple[bool, List[str]]:
    """
    Compare current results with baseline from main branch
    Returns (improved_or_same, comparison_messages)
    """
    try:
        with open(baseline_file, 'r') as f:
            baseline = json.load(f)
    except FileNotFoundError:
        print(f"⚠️  Baseline file not found: {baseline_file}")
        print("Skipping comparison, running threshold checks only...")
        return True, ["No baseline for comparison"]
    
    messages = ["\n### Comparison with Main Branch\n"]
    all_improved = True
    
    current_results = current.get("results", {})
    baseline_results = baseline.get("results", {})
    
    for metric in THRESHOLDS.keys():
        current_val = current_results.get(metric)
        baseline_val = baseline_results.get(metric)
        
        if current_val is None or baseline_val is None:
            continue
        
        # Calculate change percentage
        if baseline_val != 0:
            change_pct = ((current_val - baseline_val) / baseline_val) * 100
        else:
            change_pct = 0
        
        # Determine if improved (lower is better for all metrics)
        improved = change_pct <= 5  # Allow 5% tolerance
        
        if improved:
            status = "✅"
        elif abs(change_pct) <= 10:
            status = "⚠️ "
            all_improved = False
        else:
            status = "🚨"
            all_improved = False
        
        arrow = "↓" if change_pct < 0 else "↑" if change_pct > 0 else "="
        
        messages.append(
            f"{status} {metric}: {baseline_val:.2f} → {current_val:.2f} "
            f"({arrow}{abs(change_pct):.1f}%)"
        )
    
    return all_improved, messages


def main():
    parser = argparse.ArgumentParser(
        description='Check benchmark results for performance regressions'
    )
    parser.add_argument('benchmark_file', help='Path to benchmark_data.json')
    parser.add_argument('--baseline', help='Path to baseline benchmark file for comparison')
    parser.add_argument('--output', '-o', help='Output report file')
    parser.add_argument('--fail-on-regression', action='store_true', default=True,
                       help='Exit with error code if regressions detected')
    
    args = parser.parse_args()
    
    # Load benchmark data
    data = load_benchmark_data(args.benchmark_file)
    
    # Check against thresholds
    all_passed, messages = check_regressions(data)
    
    # Compare with baseline if provided
    if args.baseline:
        improved, comparison_msgs = compare_with_baseline(data, args.baseline)
        all_passed = all_passed and improved
        messages.extend(comparison_msgs)
    
    # Generate report
    generate_report(all_passed, messages, args.output)
    
    # Exit with appropriate code
    if not all_passed and args.fail_on_regression:
        print("\n❌ Exiting with error due to performance regressions")
        sys.exit(1)
    else:
        print("\n✅ All performance checks passed")
        sys.exit(0)


if __name__ == '__main__':
    main()
