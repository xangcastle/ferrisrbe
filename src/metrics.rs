//! Prometheus metrics of the FerrisRBE server, exported at `/metrics`.
//!
//! Metric names and label sets are part of the operational contract
//! (dashboards and alerts query them) and change only deliberately.

use std::sync::Arc;
use std::time::Duration;

use prometheus::core::Collector;
use prometheus::{
    CounterVec, Encoder, Gauge, HistogramOpts, HistogramVec, Opts, Registry, TextEncoder,
};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};
use tokio::time::interval;
use tracing::{debug, error};

use crate::cache::action_cache::ActionCacheHitTier;
use crate::worker::k8s::FleetLoad;

const PROCESS_COLLECTOR_INTERVAL_SECS: u64 = 15;

/// Buckets of BES storage work: from an uncontended lock to a compaction.
const BES_STORE_BUCKETS: &[f64] = &[
    0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0,
];

const GRPC_DURATION_BUCKETS: &[f64] = &[
    0.0001, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Buckets of one phase of an execution: from a cached-input link of a few
/// milliseconds to a bundle that runs for minutes.
const EXECUTION_PHASE_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0,
];

/// Buckets of a whole build or its critical path: from a cached no-op to a
/// cold `//...`.
const BUILD_DURATION_BUCKETS: &[f64] = &[
    1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1200.0, 1800.0, 2700.0, 3600.0, 7200.0,
];

const INPUT_BYTES_BUCKETS: &[f64] = &[
    1e3, 1e4, 1e5, 1e6, 1e7, 5e7, 1e8, 2.5e8, 5e8, 1e9, 2.5e9, 5e9, 1e10,
];

const INPUT_FILES_BUCKETS: &[f64] = &[
    1.0, 10.0, 100.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 25000.0, 50000.0, 100000.0, 250000.0,
];

const PROXY_DURATION_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0,
];

/// Registers `metric` in `registry` and returns it. Metric definitions are
/// static, so a registration failure is a programming error.
fn registered<M: Collector + Clone + 'static>(registry: &Registry, metric: M) -> M {
    registry
        .register(Box::new(metric.clone()))
        .expect("metric definitions are unique and valid");
    metric
}

fn counter_vec(registry: &Registry, name: &str, help: &str, labels: &[&str]) -> CounterVec {
    registered(
        registry,
        CounterVec::new(Opts::new(name, help), labels).expect("valid counter definition"),
    )
}

fn gauge(registry: &Registry, name: &str, help: &str) -> Gauge {
    registered(
        registry,
        Gauge::with_opts(Opts::new(name, help)).expect("valid gauge definition"),
    )
}

fn histogram_vec(
    registry: &Registry,
    name: &str,
    help: &str,
    buckets: &[f64],
    labels: &[&str],
) -> HistogramVec {
    registered(
        registry,
        HistogramVec::new(
            HistogramOpts::new(name, help).buckets(buckets.to_vec()),
            labels,
        )
        .expect("valid histogram definition"),
    )
}

/// Every metric the server exports, registered in one private registry.
#[derive(Debug)]
pub struct Metrics {
    registry: Registry,
    action_cache_requests_total: CounterVec,
    action_cache_hits_total: CounterVec,
    action_cache_misses_total: CounterVec,
    action_cache_stale_hits_total: CounterVec,
    execution_abandoned_total: CounterVec,
    action_cache_update_validation_skips_total: CounterVec,
    grpc_requests_total: CounterVec,
    grpc_request_duration_seconds: HistogramVec,
    cas_size_bytes: Gauge,
    cas_entries: Gauge,
    /// Labelled by upstream `host`, which the proxy allow-list bounds. The
    /// request path is never a label: it is unbounded and would explode the
    /// series count.
    proxy_requests_total: CounterVec,
    proxy_cache_hits_total: CounterVec,
    proxy_cache_misses_total: CounterVec,
    proxy_singleflight_coalesced_total: CounterVec,
    proxy_request_duration_seconds: HistogramVec,
    proxy_upstream_bytes_total: CounterVec,
    proxy_disk_size_bytes: Gauge,
    proxy_entries: Gauge,
    process_cpu_seconds_total: CounterVec,
    process_resident_memory_bytes: Gauge,
    process_cpu_usage_ratio: Gauge,
    execution_queued_actions: Gauge,
    execution_running_actions: Gauge,
    workers: Gauge,
    worker_slots: Gauge,
    worker_free_slots: Gauge,
    /// Labelled by `phase`: `queued`, `input_fetch`, `execution`,
    /// `output_upload`.
    execution_phase_seconds: HistogramVec,
    /// Labelled by `mnemonic`, which Bazel sends in `RequestMetadata`;
    /// `unknown` when the client sent none.
    execution_input_bytes: HistogramVec,
    execution_input_files: HistogramVec,
    /// Labelled by Bazel `command` and final `status`.
    bes_builds_total: CounterVec,
    bes_build_duration_seconds: HistogramVec,
    /// Labelled by `phase`: `lock_wait` (waiting for the BES write lock),
    /// `event` (storing one build event) and `maintenance` (reloading or
    /// compacting the histories while holding the lock).
    bes_store_seconds: HistogramVec,
    bes_critical_path_seconds: HistogramVec,
}

impl Metrics {
    /// A registry with every metric registered and every fixed-label proxy
    /// series initialised to zero.
    pub fn new() -> Self {
        let registry = Registry::new();
        let r = &registry;

        let metrics = Self {
            execution_queued_actions: gauge(
                r,
                "rbe_execution_queued_actions",
                "Actions waiting in the scheduler for a worker slot",
            ),
            execution_running_actions: gauge(
                r,
                "rbe_execution_running_actions",
                "Actions assigned to a worker and not yet reported back",
            ),
            workers: gauge(r, "rbe_workers", "Connected workers not marked unhealthy"),
            worker_slots: gauge(
                r,
                "rbe_worker_slots",
                "Execution slots across connected healthy workers",
            ),
            worker_free_slots: gauge(
                r,
                "rbe_worker_free_slots",
                "Execution slots of connected healthy workers with nothing assigned",
            ),
            execution_phase_seconds: histogram_vec(
                r,
                "rbe_execution_phase_seconds",
                "Time each executed action spent queued, fetching inputs, running and uploading outputs",
                EXECUTION_PHASE_BUCKETS,
                &["phase"],
            ),
            execution_input_bytes: histogram_vec(
                r,
                "rbe_execution_input_bytes",
                "Bytes of the input root each executed action materialized",
                INPUT_BYTES_BUCKETS,
                &["mnemonic"],
            ),
            execution_input_files: histogram_vec(
                r,
                "rbe_execution_input_files",
                "Files in the input root each executed action materialized",
                INPUT_FILES_BUCKETS,
                &["mnemonic"],
            ),
            bes_builds_total: counter_vec(
                r,
                "rbe_bes_builds_total",
                "Builds that finished streaming to the BES, by command and status",
                &["command", "status"],
            ),
            bes_store_seconds: histogram_vec(
                r,
                "rbe_bes_store_seconds",
                "Time the BES store spent waiting for its write lock, storing an event, or maintaining its histories under the lock",
                BES_STORE_BUCKETS,
                &["phase"],
            ),
            bes_build_duration_seconds: histogram_vec(
                r,
                "rbe_bes_build_duration_seconds",
                "Wall time of the builds that finished streaming to the BES",
                BUILD_DURATION_BUCKETS,
                &["command", "status"],
            ),
            bes_critical_path_seconds: histogram_vec(
                r,
                "rbe_bes_critical_path_seconds",
                "Critical path Bazel reported for each build",
                BUILD_DURATION_BUCKETS,
                &["command"],
            ),
            action_cache_requests_total: counter_vec(
                r,
                "rbe_action_cache_requests_total",
                "Total Action Cache requests by method",
                &["method"],
            ),
            action_cache_hits_total: counter_vec(
                r,
                "rbe_action_cache_hits_total",
                "Total Action Cache hits by tier",
                &["tier"],
            ),
            action_cache_misses_total: counter_vec(
                r,
                "rbe_action_cache_misses_total",
                "Total Action Cache misses",
                &[],
            ),
            action_cache_stale_hits_total: counter_vec(
                r,
                "rbe_action_cache_stale_hits_total",
                "Action Cache hits dropped because a referenced blob was gone from the CAS",
                &[],
            ),
            execution_abandoned_total: counter_vec(
                r,
                "rbe_execution_abandoned_total",
                "Queued operations cancelled because no client followed them",
                &[],
            ),
            action_cache_update_validation_skips_total: counter_vec(
                r,
                "rbe_action_cache_update_validation_skips_total",
                "Total Action Cache updates skipped during validation",
                &["reason"],
            ),
            grpc_requests_total: counter_vec(
                r,
                "rbe_grpc_requests_total",
                "Total gRPC requests by service, method and status",
                &["service", "method", "status"],
            ),
            grpc_request_duration_seconds: histogram_vec(
                r,
                "rbe_grpc_request_duration_seconds",
                "gRPC request duration by service and method",
                GRPC_DURATION_BUCKETS,
                &["service", "method"],
            ),
            cas_size_bytes: gauge(r, "rbe_cas_size_bytes", "Current CAS size in bytes"),
            cas_entries: gauge(r, "rbe_cas_entries", "Current number of CAS entries"),
            proxy_requests_total: counter_vec(
                r,
                "rbe_proxy_requests_total",
                "Total pull-through proxy requests by upstream host and outcome",
                &["host", "status"],
            ),
            proxy_cache_hits_total: counter_vec(
                r,
                "rbe_proxy_cache_hits_total",
                "Pull-through proxy cache hits by asset class",
                &["class"],
            ),
            proxy_cache_misses_total: counter_vec(
                r,
                "rbe_proxy_cache_misses_total",
                "Pull-through proxy cache misses by asset class",
                &["class"],
            ),
            proxy_singleflight_coalesced_total: counter_vec(
                r,
                "rbe_proxy_singleflight_coalesced_total",
                "Requests merged into an in-flight fetch instead of hitting the origin",
                &[],
            ),
            proxy_request_duration_seconds: histogram_vec(
                r,
                "rbe_proxy_request_duration_seconds",
                "Pull-through proxy request duration by upstream host",
                PROXY_DURATION_BUCKETS,
                &["host"],
            ),
            proxy_upstream_bytes_total: counter_vec(
                r,
                "rbe_proxy_upstream_bytes_total",
                "Bytes downloaded from origins, i.e. traffic the cache did not save",
                &[],
            ),
            proxy_disk_size_bytes: gauge(
                r,
                "rbe_proxy_disk_size_bytes",
                "Bytes held by the pull-through cache on disk",
            ),
            proxy_entries: gauge(
                r,
                "rbe_proxy_entries",
                "Number of artifacts in the pull-through cache",
            ),
            process_cpu_seconds_total: counter_vec(
                r,
                "process_cpu_seconds_total",
                "Total user and system CPU time spent by the process in seconds",
                &[],
            ),
            process_resident_memory_bytes: gauge(
                r,
                "process_resident_memory_bytes",
                "Resident memory size of the process in bytes",
            ),
            process_cpu_usage_ratio: gauge(
                r,
                "process_cpu_usage_ratio",
                "Current CPU usage ratio of the process (0-1)",
            ),
            registry,
        };

        metrics.initialize_proxy_series();
        metrics
    }

    /// Exports the proxy counter series whose label sets are known upfront at
    /// zero.
    ///
    /// A `CounterVec` exports nothing before its first increment; without this
    /// a fresh process would show hit-ratio panels as "No data" rather than
    /// zero, and `rate()` alerts on these counters would evaluate to nothing
    /// until the first event.
    fn initialize_proxy_series(&self) {
        self.proxy_singleflight_coalesced_total
            .with_label_values(&[])
            .inc_by(0.0);
        self.proxy_upstream_bytes_total
            .with_label_values(&[])
            .inc_by(0.0);
        for class in ["immutable", "mutable"] {
            self.proxy_cache_hits_total
                .with_label_values(&[class])
                .inc_by(0.0);
            self.proxy_cache_misses_total
                .with_label_values(&[class])
                .inc_by(0.0);
        }
    }

    /// Record the outcome and latency of a pull-through proxy request.
    pub fn record_proxy_request(&self, host: &str, status: &str, duration: Duration) {
        self.proxy_requests_total
            .with_label_values(&[host, status])
            .inc();
        self.proxy_request_duration_seconds
            .with_label_values(&[host])
            .observe(duration.as_secs_f64());
    }

    /// Record a pull-through cache hit for the given asset class.
    pub fn record_proxy_hit(&self, class: &str) {
        self.proxy_cache_hits_total
            .with_label_values(&[class])
            .inc();
    }

    /// Record a pull-through cache miss for the given asset class.
    pub fn record_proxy_miss(&self, class: &str) {
        self.proxy_cache_misses_total
            .with_label_values(&[class])
            .inc();
    }

    /// Record a request that joined an in-flight fetch instead of hitting the origin.
    pub fn record_proxy_coalesced(&self) {
        self.proxy_singleflight_coalesced_total
            .with_label_values(&[])
            .inc();
    }

    /// Record bytes pulled from an origin.
    pub fn record_proxy_upstream_bytes(&self, bytes: u64) {
        self.proxy_upstream_bytes_total
            .with_label_values(&[])
            .inc_by(bytes as f64);
    }

    /// Publish the pull-through cache's disk footprint.
    pub fn record_proxy_disk(&self, bytes: u64, entries: u64) {
        self.proxy_disk_size_bytes.set(bytes as f64);
        self.proxy_entries.set(entries as f64);
    }

    /// Render all metrics in Prometheus text format.
    pub fn gather(&self) -> String {
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        let encoder = TextEncoder::new();
        if let Err(e) = encoder.encode(&metric_families, &mut buffer) {
            error!("Failed to encode Prometheus metrics: {}", e);
            return String::new();
        }
        String::from_utf8_lossy(&buffer).into_owned()
    }

    /// Record an Action Cache get request and its outcome tier.
    pub fn record_action_cache_get(&self, tier: ActionCacheHitTier) {
        self.action_cache_requests_total
            .with_label_values(&["get"])
            .inc();
        match tier {
            ActionCacheHitTier::L1 => self
                .action_cache_hits_total
                .with_label_values(&["l1"])
                .inc(),
            ActionCacheHitTier::L2 => self
                .action_cache_hits_total
                .with_label_values(&["l2"])
                .inc(),
            ActionCacheHitTier::Remote => self
                .action_cache_hits_total
                .with_label_values(&["remote"])
                .inc(),
            ActionCacheHitTier::Miss => self.action_cache_misses_total.with_label_values(&[]).inc(),
        }
    }

    /// Record `count` queued operations cancelled for lack of watchers.
    pub fn record_execution_abandoned(&self, count: usize) {
        self.execution_abandoned_total
            .with_label_values(&[])
            .inc_by(count as f64);
    }

    /// Record an Action Cache hit discarded because its outputs left the CAS.
    pub fn record_action_cache_stale_hit(&self) {
        self.action_cache_stale_hits_total
            .with_label_values(&[])
            .inc();
    }

    /// Record an Action Cache update request and, if it was not stored, the
    /// reason it was skipped.
    pub fn record_action_cache_update(&self, skipped: Option<&str>) {
        self.action_cache_requests_total
            .with_label_values(&["update"])
            .inc();
        if let Some(reason) = skipped {
            self.action_cache_update_validation_skips_total
                .with_label_values(&[reason])
                .inc();
        }
    }

    /// Record a completed gRPC request.
    pub fn record_grpc_request(
        &self,
        service: &str,
        method: &str,
        duration: Duration,
        status: &str,
    ) {
        self.grpc_requests_total
            .with_label_values(&[service, method, status])
            .inc();
        self.grpc_request_duration_seconds
            .with_label_values(&[service, method])
            .observe(duration.as_secs_f64());
    }

    /// Record the scheduler queue length and the worker fleet's load.
    pub fn record_execution_load(&self, queued: usize, load: FleetLoad) {
        self.execution_queued_actions.set(queued as f64);
        self.execution_running_actions.set(load.running as f64);
        self.workers.set(load.workers as f64);
        self.worker_slots.set(load.slots as f64);
        self.worker_free_slots.set(load.free_slots as f64);
    }

    /// Record how long one executed action spent in `phase`.
    pub fn record_execution_phase(&self, phase: &str, duration: Duration) {
        self.execution_phase_seconds
            .with_label_values(&[phase])
            .observe(duration.as_secs_f64());
    }

    /// Record the input root one executed action materialized.
    pub fn record_execution_inputs(&self, mnemonic: &str, files: u64, bytes: u64) {
        let mnemonic = if mnemonic.is_empty() {
            "unknown"
        } else {
            mnemonic
        };
        self.execution_input_bytes
            .with_label_values(&[mnemonic])
            .observe(bytes as f64);
        self.execution_input_files
            .with_label_values(&[mnemonic])
            .observe(files as f64);
    }

    /// Record one `phase` of BES storage work.
    pub fn record_bes_store(&self, phase: &str, duration: Duration) {
        self.bes_store_seconds
            .with_label_values(&[phase])
            .observe(duration.as_secs_f64());
    }

    /// Record a build whose stream reached its final status.
    pub fn record_bes_build(&self, command: &str, status: &str, duration: Duration) {
        self.bes_builds_total
            .with_label_values(&[command, status])
            .inc();
        self.bes_build_duration_seconds
            .with_label_values(&[command, status])
            .observe(duration.as_secs_f64());
    }

    /// Record the critical path Bazel reported for a build.
    pub fn record_bes_critical_path(&self, command: &str, duration: Duration) {
        self.bes_critical_path_seconds
            .with_label_values(&[command])
            .observe(duration.as_secs_f64());
    }

    /// Record current CAS size and entry count.
    pub fn record_cas_stats(&self, bytes: u64, entries: u64) {
        self.cas_size_bytes.set(bytes as f64);
        self.cas_entries.set(entries as f64);
    }

    /// Spawns a task that samples this process's CPU and resident memory every
    /// 15 seconds for the `process_*` metrics.
    pub fn spawn_process_collector(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut system = System::new_with_specifics(
                RefreshKind::nothing().with_processes(ProcessRefreshKind::everything()),
            );
            let pid = Pid::from_u32(std::process::id());
            let mut interval = interval(Duration::from_secs(PROCESS_COLLECTOR_INTERVAL_SECS));

            loop {
                interval.tick().await;
                system.refresh_processes_specifics(
                    ProcessesToUpdate::Some(&[pid]),
                    false,
                    ProcessRefreshKind::everything(),
                );

                if let Some(process) = system.process(pid) {
                    let memory = process.memory();
                    let cpu_percent_since_last_sample = process.cpu_usage() as f64;
                    let cpu_ratio = cpu_percent_since_last_sample / 100.0;
                    let interval_seconds = PROCESS_COLLECTOR_INTERVAL_SECS as f64;
                    let cpu_seconds_delta = cpu_ratio * interval_seconds;

                    self.process_resident_memory_bytes.set(memory as f64);
                    self.process_cpu_usage_ratio.set(cpu_ratio);
                    self.process_cpu_seconds_total
                        .with_label_values(&[])
                        .inc_by(cpu_seconds_delta);

                    debug!(
                        "Process metrics updated: memory={} bytes, cpu_usage={:.2}%, cpu_seconds_delta={:.4}",
                        memory, cpu_percent_since_last_sample, cpu_seconds_delta
                    );
                } else {
                    error!("Failed to read current process info for metrics");
                }
            }
        });
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_execution_load_is_exported_under_the_names_the_autoscaler_reads() {
        let metrics = Metrics::new();
        metrics.record_execution_load(
            7,
            FleetLoad {
                workers: 2,
                slots: 32,
                free_slots: 5,
                running: 27,
            },
        );

        let text = metrics.gather();
        for line in [
            "rbe_execution_queued_actions 7",
            "rbe_execution_running_actions 27",
            "rbe_workers 2",
            "rbe_worker_slots 32",
            "rbe_worker_free_slots 5",
        ] {
            assert!(
                text.lines().any(|l| l == line),
                "missing {line:?} in\n{text}"
            );
        }
    }

    #[test]
    fn action_cache_metrics_are_exported_by_tier_and_reason() {
        let metrics = Metrics::new();

        metrics.record_action_cache_get(ActionCacheHitTier::L1);
        metrics.record_action_cache_get(ActionCacheHitTier::L2);
        metrics.record_action_cache_get(ActionCacheHitTier::Miss);
        metrics.record_action_cache_update(None);
        metrics.record_action_cache_update(Some("missing_blobs"));

        let output = metrics.gather();
        assert!(output.contains("rbe_action_cache_requests_total{method=\"get\"}"));
        assert!(output.contains("rbe_action_cache_hits_total{tier=\"l1\"}"));
        assert!(output.contains("rbe_action_cache_hits_total{tier=\"l2\"}"));
        assert!(output.contains("rbe_action_cache_misses_total"));
        assert!(output
            .contains("rbe_action_cache_update_validation_skips_total{reason=\"missing_blobs\"}"));
    }

    #[test]
    fn grpc_metrics_are_exported_by_service_method_and_status() {
        let metrics = Metrics::new();
        metrics.record_grpc_request("cas", "FindMissingBlobs", Duration::from_millis(5), "ok");

        let output = metrics.gather();
        assert!(output.contains(
            "rbe_grpc_requests_total{method=\"FindMissingBlobs\",service=\"cas\",status=\"ok\"}"
        ));
        assert!(output.contains(
            "rbe_grpc_request_duration_seconds_bucket{method=\"FindMissingBlobs\",service=\"cas\""
        ));
    }

    #[test]
    fn build_and_input_metrics_are_exported_by_their_labels() {
        let metrics = Metrics::new();
        metrics.record_bes_build("build", "success", Duration::from_secs(42));
        metrics.record_bes_critical_path("build", Duration::from_secs(9));
        metrics.record_execution_inputs("ViteBuild", 35_492, 300_000_000);
        metrics.record_execution_inputs("", 2, 10);

        let output = metrics.gather();
        assert!(output.contains("rbe_bes_builds_total{command=\"build\",status=\"success\"} 1"));
        assert!(output.contains(
            "rbe_bes_build_duration_seconds_sum{command=\"build\",status=\"success\"} 42"
        ));
        assert!(output.contains("rbe_bes_critical_path_seconds_sum{command=\"build\"} 9"));
        assert!(output.contains("rbe_execution_input_files_sum{mnemonic=\"ViteBuild\"} 35492"));
        assert!(output.contains("rbe_execution_input_bytes_count{mnemonic=\"unknown\"} 1"));
    }

    #[test]
    fn fixed_label_proxy_series_are_exported_before_any_request() {
        let output = Metrics::new().gather();
        assert!(output.contains("rbe_proxy_cache_hits_total{class=\"immutable\"} 0"));
        assert!(output.contains("rbe_proxy_cache_misses_total{class=\"mutable\"} 0"));
        assert!(output.contains("rbe_proxy_upstream_bytes_total 0"));
    }
}
