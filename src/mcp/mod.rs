//! Embedded MCP server exposing FerrisRBE build data as tools.
//!
//! Tools run in-process against `BesStorage`, `WorkerRegistry` and `Metrics`
//! — no HTTP hop, no gRPC. Mounted at `/mcp` as a stateless streamable-HTTP
//! Tower service so any MCP client can inspect builds, events, logs, cache
//! misses, targets, tests, CAS blobs and Prometheus metrics.
//!
//! The tool surface is curated for the calling agent (the descriptions are
//! the agent's entire documentation): list tools page with `limit`/`offset`
//! (clamped into `[1, MAX_LIMIT]` rather than rejected) and every paged
//! response carries the unpaginated `total`, so a truncated page is never
//! mistaken for the whole list. Parameter schemas are strict
//! (`additionalProperties: false`): unknown arguments are rejected rather
//! than silently ignored.

mod prometheus;

use std::collections::BTreeMap;
use std::sync::Arc;

use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, ContentBlock, Implementation, InitializeResult, ServerCapabilities,
        ServerConfig,
    },
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use serde::{Deserialize, Serialize};

use crate::bes::config::is_invocation_id;
use crate::bes::profile::{
    BuildProfile, CriticalStep, Executor, ExecutorTotal, ProfileAction, ProfileLookup, ProfilePhase,
};
use crate::bes::storage::BesStorage;
use crate::cas::SharedCasBackend;
use crate::metrics::Metrics;
use crate::proxy::PullThroughCache;
use crate::types::DigestInfo;
use crate::worker::k8s::WorkerRegistry;
use prometheus::{parse_duration, PrometheusClient};

/// Default page size for the list tools. The bound exists for the caller's
/// context, not for the server.
const DEFAULT_LIMIT: usize = 100;

/// Maximum page size. `limit` is clamped into range instead of erroring, so
/// an agent learns the bound from the schema rather than a rejected call.
const MAX_LIMIT: usize = 500;

/// A paged list response. `total` is the unpaginated record count: a page
/// shorter than the limit with a larger `total` means "keep paging", never
/// "this is everything".
#[derive(Serialize)]
struct Paged<T> {
    total: usize,
    items: Vec<T>,
}

/// The page of `items` selected by `limit` and `offset`, with `limit`
/// defaulted to [`DEFAULT_LIMIT`] and clamped into `[1, MAX_LIMIT]`.
fn paginate<T>(items: Vec<T>, limit: Option<usize>, offset: Option<usize>) -> Paged<T> {
    let total = items.len();
    let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let mut page = Vec::with_capacity(limit);
    page.extend(items.into_iter().skip(offset.unwrap_or(0)).take(limit));
    Paged { total, items: page }
}

/// A successful tool result carrying `content` as text.
fn text(content: impl Into<String>) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::text(
        content.into(),
    )]))
}

/// A successful tool result carrying `value` as pretty-printed JSON.
fn json_text<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let pretty = serde_json::to_string_pretty(value)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    text(pretty)
}

/// An internal MCP error carrying `error`'s message.
fn internal<E: std::fmt::Display>(error: E) -> McpError {
    McpError::internal_error(error.to_string(), None)
}

/// A build id that is an invocation id ([`is_invocation_id`]); any other
/// is refused before it can reach a path under the BES data directory.
fn invocation_id<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let id = String::deserialize(deserializer)?;
    if is_invocation_id(&id) {
        Ok(id)
    } else {
        Err(serde::de::Error::custom(format!("invalid build id {id:?}")))
    }
}

/// An invalid-params MCP error, for references to data that does not exist.
fn not_found(message: String) -> McpError {
    McpError::invalid_params(message, None)
}

/// The MCP server: one tool per read-only view of the build data.
#[derive(Clone)]
pub struct BesMcp {
    storage: BesStorage,
    cas: Option<SharedCasBackend>,
    workers: Option<Arc<WorkerRegistry>>,
    proxy: Option<Arc<PullThroughCache>>,
    metrics: Arc<Metrics>,
    /// The cluster's Prometheus, for `rbe_prometheus`; `None` disables it.
    prometheus: Option<PrometheusClient>,
    tool_router: ToolRouter<BesMcp>,
}

#[tool_router]
impl BesMcp {
    pub fn new(
        storage: BesStorage,
        cas: Option<SharedCasBackend>,
        workers: Option<Arc<WorkerRegistry>>,
        proxy: Option<Arc<PullThroughCache>>,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            storage,
            cas,
            workers,
            proxy,
            metrics,
            prometheus: None,
            tool_router: Self::tool_router(),
        }
    }

    /// Answers `rbe_prometheus` from `client`.
    pub fn with_prometheus(mut self, client: Option<PrometheusClient>) -> Self {
        self.prometheus = client;
        self
    }

    #[tool(
        description = "Global build statistics: total/in-progress/successful/failed builds and total/failed actions. Read this first to know what to look for before listing builds or targets."
    )]
    async fn rbe_stats(&self) -> Result<CallToolResult, McpError> {
        let stats = self.storage.stats().await.map_err(internal)?;
        json_text(&stats)
    }

    #[tool(
        description = "List recorded build invocations, newest first, with status, origin and metadata. The invocation id is the UUID Bazel prints — every other build tool takes it. Paged: limit (default 100, maximum 500) and offset; the response carries the unpaginated `total`."
    )]
    async fn rbe_builds(
        &self,
        Parameters(ListParams { limit, offset }): Parameters<ListParams>,
    ) -> Result<CallToolResult, McpError> {
        let builds = self.storage.list_builds().await.map_err(internal)?;
        json_text(&paginate(builds, limit, offset))
    }

    #[tool(
        description = "Fetch one build invocation summary by invocation id (the UUID Bazel prints). Its `runners` split tells where the spawns ran: `remote` on the RBE workers, sandboxes/`local`/`worker` on the machine that ran Bazel (the CI runner or a laptop), `* cache hit` served from a cache. An unknown id returns an empty summary, not an error — resolve ids with rbe_builds first."
    )]
    async fn rbe_build(
        &self,
        Parameters(BuildParams { id }): Parameters<BuildParams>,
    ) -> Result<CallToolResult, McpError> {
        match self.storage.get_build(&id).await.map_err(internal)? {
            Some(build) => json_text(&build),
            None => json_text(&crate::bes::models::BuildSummary {
                invocation_id: id,
                ..Default::default()
            }),
        }
    }

    #[tool(
        description = "Fetch the build events of one invocation in stream order, including its failure events. Set important=false to get every event instead of the command/metadata/failure/finish ones. Action output digests in these events feed rbe_blob. Paged: limit (default 100, maximum 500) and offset; the response carries the unpaginated `total`."
    )]
    async fn rbe_build_events(
        &self,
        Parameters(EventsParams {
            id,
            important,
            limit,
            offset,
        }): Parameters<EventsParams>,
    ) -> Result<CallToolResult, McpError> {
        let events = self
            .storage
            .get_events(&id, important.unwrap_or(true))
            .await
            .map_err(internal)?;
        json_text(&paginate(events, limit, offset))
    }

    #[tool(
        description = "Fetch the reconstructed console log of one build invocation as plain text — usually the fastest way to see why a build failed."
    )]
    async fn rbe_build_log(
        &self,
        Parameters(BuildParams { id }): Parameters<BuildParams>,
    ) -> Result<CallToolResult, McpError> {
        text(self.storage.get_console(&id).await.map_err(internal)?)
    }

    #[tool(description = "Fetch the per-target cache-miss report of one build invocation.")]
    async fn rbe_build_misses(
        &self,
        Parameters(BuildParams { id }): Parameters<BuildParams>,
    ) -> Result<CallToolResult, McpError> {
        let misses = self.storage.get_misses(&id).await.map_err(internal)?;
        json_text(&misses)
    }

    #[tool(
        description = "Fetch the per-target execution detail (actions, cache hits, durations, heaviest inputs) of one build invocation. sort: label (default) or duration (longest action time first). Paged: limit (default 100, maximum 500) and offset; the response carries the unpaginated `total`."
    )]
    async fn rbe_build_targets(
        &self,
        Parameters(BuildTargetsParams {
            id,
            limit,
            offset,
            sort,
        }): Parameters<BuildTargetsParams>,
    ) -> Result<CallToolResult, McpError> {
        let mut targets = self
            .storage
            .get_build_targets(&id)
            .await
            .map_err(internal)?;
        match sort.as_deref().unwrap_or("label") {
            "label" => {}
            "duration" => targets.sort_by(|a, b| b.action_duration_ms.cmp(&a.action_duration_ms)),
            other => {
                return Err(not_found(format!(
                    "unknown sort {other:?}: label or duration"
                )))
            }
        }
        json_text(&paginate(targets, limit, offset))
    }

    #[tool(
        description = "List every target ever seen with aggregate executions, failures and action time over the retained builds: total_action_ms, max_action_ms and mean_action_ms (the mean over the executions that ran actions, so cache hits do not dilute it). sort: label (default), mean_duration, max_duration, total_duration, executions or failures; every order but label is descending — mean_duration lists the heaviest targets to build. Paged: limit (default 100, maximum 500) and offset; the response carries the unpaginated `total`."
    )]
    async fn rbe_targets(
        &self,
        Parameters(TargetsParams {
            limit,
            offset,
            sort,
        }): Parameters<TargetsParams>,
    ) -> Result<CallToolResult, McpError> {
        let mut targets = self.storage.list_targets().await.map_err(internal)?;
        targets.sort_by(|a, b| a.label.cmp(&b.label));
        let key: fn(&crate::bes::models::TargetSummary) -> u64 = match sort.as_deref().unwrap_or("label") {
            "label" => |_| 0,
            "mean_duration" => |t| t.mean_action_ms,
            "max_duration" => |t| t.max_action_ms,
            "total_duration" => |t| t.total_action_ms,
            "executions" => |t| t.total_executions as u64,
            "failures" => |t| t.failure_count as u64,
            other => {
                return Err(not_found(format!(
                    "unknown sort {other:?}: label, mean_duration, max_duration, total_duration, executions or failures"
                )))
            }
        };
        targets.sort_by_key(|t| std::cmp::Reverse(key(t)));
        json_text(&paginate(targets, limit, offset))
    }

    #[tool(
        description = "Fetch the executions of one target by its Bazel label, newest build first, each with its status, action count and duration. Paged: limit (default 100, maximum 500) and offset; the response carries the unpaginated `total`."
    )]
    async fn rbe_target_history(
        &self,
        Parameters(HistoryParams {
            label,
            limit,
            offset,
        }): Parameters<HistoryParams>,
    ) -> Result<CallToolResult, McpError> {
        let mut history = self
            .storage
            .get_target_history(&label)
            .await
            .map_err(internal)?;
        history.reverse();
        json_text(&paginate(history, limit, offset))
    }

    #[tool(
        description = "List every test target ever seen, sorted by label, with aggregate executions, failures and flakiness. Paged: limit (default 100, maximum 500) and offset; the response carries the unpaginated `total`."
    )]
    async fn rbe_tests(
        &self,
        Parameters(ListParams { limit, offset }): Parameters<ListParams>,
    ) -> Result<CallToolResult, McpError> {
        let mut tests = self.storage.list_tests().await.map_err(internal)?;
        tests.sort_by(|a, b| a.label.cmp(&b.label));
        json_text(&paginate(tests, limit, offset))
    }

    #[tool(
        description = "Fetch the runs of one test target by its Bazel label, newest first, each with its status, attempts and duration. Paged: limit (default 100, maximum 500) and offset; the response carries the unpaginated `total`."
    )]
    async fn rbe_test_history(
        &self,
        Parameters(HistoryParams {
            label,
            limit,
            offset,
        }): Parameters<HistoryParams>,
    ) -> Result<CallToolResult, McpError> {
        let mut history = self
            .storage
            .get_test_history(&label)
            .await
            .map_err(internal)?;
        history.reverse();
        json_text(&paginate(history, limit, offset))
    }

    #[tool(
        description = "Read a blob from the CAS (e.g. the stdout/stderr of a failed action) by its sha256 hash and byte size, returned as text. Digests come from the action results in rbe_build_events or rbe_build_targets."
    )]
    async fn rbe_blob(
        &self,
        Parameters(BlobParams { hash, size }): Parameters<BlobParams>,
    ) -> Result<CallToolResult, McpError> {
        let Some(cas) = &self.cas else {
            return Err(McpError::internal_error("no CAS backend configured", None));
        };
        let digest = DigestInfo::new(&hash, size).map_err(|e| not_found(e.to_string()))?;
        match cas.read(&digest).await.map_err(internal)? {
            Some(bytes) => text(String::from_utf8_lossy(&bytes).into_owned()),
            None => Err(not_found(format!("blob {hash}/{size} not found"))),
        }
    }

    #[tool(
        description = "Fetch what one build was started with: the command line as typed, the options Bazel parsed (rc files included) and those given explicitly, the --client_env and the workspace status. Secret values are redacted. Use it to tell whether two builds of one target ran with the same flags."
    )]
    async fn rbe_build_inputs(
        &self,
        Parameters(BuildParams { id }): Parameters<BuildParams>,
    ) -> Result<CallToolResult, McpError> {
        match self.storage.get_build_inputs(&id).await.map_err(internal)? {
            Some(inputs) => json_text(&inputs),
            None => Err(not_found(format!("build {id} not found"))),
        }
    }

    #[tool(
        description = "Summarize the Bazel profile of one build (needs --generate_json_trace_profile, Bazel's default, and the profile uploaded to this server's CAS): total time, phase markers, the critical path step by step, the `top` longest actions (default 20, maximum 200) with mnemonic, target, output and executor, totals per executor, and per-mnemonic totals with where each ran. Remote actions name their worker pod, with totals per worker. An executor is `remote` (RBE workers), `sandbox` or `local` (the machine that ran Bazel: the CI runner or a laptop), `cache` or `internal` (no process). Answers why a build was slow and which actions to cache or split."
    )]
    async fn rbe_build_profile(
        &self,
        Parameters(ProfileParams { id, top }): Parameters<ProfileParams>,
    ) -> Result<CallToolResult, McpError> {
        let Some(cas) = &self.cas else {
            return Err(McpError::internal_error("no CAS backend configured", None));
        };
        match crate::bes::profile::load(&self.storage, cas, &id)
            .await
            .map_err(internal)?
        {
            ProfileLookup::Found(profile) => json_text(&ProfileDigest::new(
                profile,
                top.unwrap_or(20).clamp(1, 200),
            )),
            ProfileLookup::NotAnnounced => {
                Err(not_found(format!("build {id} announced no profile")))
            }
            ProfileLookup::Evicted => Err(not_found(format!(
                "the profile of build {id} is no longer in the CAS"
            ))),
            ProfileLookup::Unreadable(e) => Err(internal(e)),
        }
    }

    #[tool(
        description = "Snapshot the workers currently registered for remote execution: id, type, labels, state, current execution and heartbeat age."
    )]
    async fn rbe_workers(&self) -> Result<CallToolResult, McpError> {
        match &self.workers {
            Some(registry) => json_text(&registry.snapshot()),
            None => json_text(&Vec::<crate::worker::k8s::WorkerView>::new()),
        }
    }

    #[tool(
        description = "List the artifacts cached by the fetch-phase pull-through proxy (wheels, npm tarballs, Go modules, crates, jars, Bazel modules, GitHub releases, images, toolchains). Each entry carries the upstream URL, download count, byte size, fetch time, whether it is immutable, and `artifact`: ecosystem, kind (package, index, toolchain, release, source, image), package name, version and docs_url. `ecosystems` totals artifacts, bytes and downloads per ecosystem. Filters: q (substring of URL or package name), ecosystem (python, npm, go, rust, java, bazel, github, docker, node, other). sort: downloads (default), size, newest or name. Paged: limit (default 100, maximum 500) and offset; the response carries the unpaginated `total`."
    )]
    async fn rbe_externals(
        &self,
        Parameters(ExternalsParams {
            limit,
            offset,
            q,
            ecosystem,
            sort,
        }): Parameters<ExternalsParams>,
    ) -> Result<CallToolResult, McpError> {
        let Some(proxy) = &self.proxy else {
            return json_text(&Paged {
                total: 0,
                items: Vec::<crate::proxy::ProxyEntrySnapshot>::new(),
            });
        };
        let ecosystem = match ecosystem.as_deref().filter(|e| !e.is_empty()) {
            None => None,
            Some(value) => Some(
                crate::proxy::artifact::Ecosystem::parse(value)
                    .ok_or_else(|| not_found(format!("unknown ecosystem {value:?}")))?,
            ),
        };
        let page = proxy
            .list_externals_page(
                limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
                offset.unwrap_or(0),
                q.as_deref().unwrap_or(""),
                ecosystem,
                crate::proxy::ExternalsSort::parse(sort.as_deref().unwrap_or("")),
            )
            .await;
        json_text(&page)
    }

    #[tool(
        description = "Fetch Prometheus metrics of this server (rbe_* for cache/proxy/gRPC, process_* for resources). Pass filter to keep only lines containing that substring."
    )]
    async fn rbe_metrics(
        &self,
        Parameters(MetricsParams { filter }): Parameters<MetricsParams>,
    ) -> Result<CallToolResult, McpError> {
        let gathered = self.metrics.gather();
        match filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
            Some(filter) => {
                let mut out = String::new();
                for line in gathered.lines() {
                    if line.contains('#') || line.contains(filter) {
                        out.push_str(line);
                        out.push('\n');
                    }
                }
                text(out)
            }
            None => text(gathered),
        }
    }

    #[tool(
        description = "Run a read-only PromQL query against the cluster's Prometheus, which keeps the history of every rbe_* series across server restarts (rbe_metrics only covers the running process). Without `range` it is an instant query; with `range` (30m, 6h, 7d, 2w) it returns each series over that window ending now, one point every `step` (default range/120, at least 15s, at most 500 points). At most 50 series come back; truncated_series counts the rest. This server is the job ferrisrbe-cache. Examples: histogram_quantile(0.9, sum by (le) (rate(rbe_execution_phase_seconds_bucket{phase=\"queued\"}[1h]))) for the p90 queue wait, increase(rbe_execution_abandoned_total[1d]) for cancelled operations per day."
    )]
    async fn rbe_prometheus(
        &self,
        Parameters(PrometheusParams { query, range, step }): Parameters<PrometheusParams>,
    ) -> Result<CallToolResult, McpError> {
        let Some(prometheus) = &self.prometheus else {
            return Err(McpError::internal_error(
                format!(
                    "{} is not set on this server",
                    prometheus::ENV_PROMETHEUS_URL
                ),
                None,
            ));
        };
        let parse = |value: Option<String>| {
            value
                .map(|v| parse_duration(&v))
                .transpose()
                .map_err(|e| McpError::invalid_params(e, None))
        };
        let (range, step) = (parse(range)?, parse(step)?);
        let data = prometheus
            .query(&query, range, step)
            .await
            .map_err(|e| McpError::invalid_params(e, None))?;
        json_text(&data)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BesMcp {
    fn get_info(&self) -> ServerConfig {
        let mut info = InitializeResult::new(ServerCapabilities::builder().enable_tools().build());
        info.server_info =
            Implementation::new("ferrisrbe-build-results", env!("CARGO_PKG_VERSION"))
                .with_title("FerrisRBE build results");
        info.instructions = Some(
            "Read-only build data from this FerrisRBE server. Builds are addressed by the \
             invocation id Bazel prints (a UUID); resolve one from rbe_builds if you only have a \
             time window. The list tools (rbe_builds, rbe_targets, rbe_tests, rbe_externals) and \
             the per-build and per-label ones (rbe_build_events, rbe_build_targets, \
             rbe_target_history, rbe_test_history) page with limit (default 100, maximum 500) and offset, and every response carries the \
             unpaginated `total`. For a failed build, read rbe_build_log and rbe_build_misses; \
             action stdout/stderr blobs live in the CAS and are readable with rbe_blob using the \
             digests from rbe_build_events or rbe_build_targets. For a slow build, read \
             rbe_build_profile (critical path, longest actions) and rbe_build_targets (the \
             heaviest inputs of each target); rbe_build_inputs tells which flags it ran with. \
             rbe_targets with sort=mean_duration ranks the heaviest targets to build."
                .to_string(),
        );
        info
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProfileParams {
    /// BES invocation id of the build — the UUID Bazel prints.
    #[serde(deserialize_with = "invocation_id")]
    #[schemars(with = "String")]
    id: String,
    /// How many of the longest actions to list (default 20, maximum 200).
    top: Option<usize>,
}

/// Totals of one mnemonic over the actions a profile summary kept.
#[derive(Debug, Serialize, PartialEq)]
struct MnemonicTotal {
    mnemonic: String,
    actions: usize,
    total_ms: f64,
    max_ms: f64,
    /// Actions per executor: where this mnemonic ran.
    executors: BTreeMap<Executor, usize>,
}

/// A profile summary cut to what an agent can read: the critical path, the
/// longest actions and per-mnemonic totals instead of thousands of actions.
#[derive(Debug, Serialize)]
struct ProfileDigest {
    total_ms: f64,
    lanes: u32,
    phases: Vec<ProfilePhase>,
    critical_path_ms: f64,
    critical_path: Vec<CriticalStep>,
    longest_actions: Vec<ProfileAction>,
    /// Actions and time per executor (remote = RBE workers; sandbox/local =
    /// the machine that ran Bazel; cache; internal), over every action.
    executors: BTreeMap<Executor, ExecutorTotal>,
    /// Actions and time per RBE worker pod, over every remote action matched.
    workers: BTreeMap<String, ExecutorTotal>,
    /// Longest total first, over the kept actions only.
    mnemonics: Vec<MnemonicTotal>,
    /// Actions the profile summary dropped for being short; the totals
    /// leave them out.
    omitted_actions: usize,
}

impl ProfileDigest {
    fn new(profile: BuildProfile, top: usize) -> Self {
        let mut mnemonics: Vec<MnemonicTotal> = Vec::new();
        for action in &profile.actions {
            match mnemonics.iter_mut().find(|m| m.mnemonic == action.mnemonic) {
                Some(total) => {
                    total.actions += 1;
                    total.total_ms += action.duration_ms;
                    total.max_ms = total.max_ms.max(action.duration_ms);
                    *total.executors.entry(action.executor).or_default() += 1;
                }
                None => mnemonics.push(MnemonicTotal {
                    mnemonic: action.mnemonic.clone(),
                    actions: 1,
                    total_ms: action.duration_ms,
                    max_ms: action.duration_ms,
                    executors: BTreeMap::from([(action.executor, 1)]),
                }),
            }
        }
        mnemonics.sort_by(|a, b| b.total_ms.total_cmp(&a.total_ms));
        let mut longest_actions = profile.actions;
        longest_actions.sort_by(|a, b| b.duration_ms.total_cmp(&a.duration_ms));
        longest_actions.truncate(top);
        Self {
            total_ms: profile.total_ms,
            lanes: profile.lanes,
            phases: profile.phases,
            critical_path_ms: profile
                .critical_path
                .iter()
                .fold(0.0, |ms, s| ms + s.duration_ms),
            critical_path: profile.critical_path,
            longest_actions,
            executors: profile.executors,
            workers: profile.workers,
            mnemonics,
            omitted_actions: profile.omitted_actions,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct BuildParams {
    /// BES invocation id of the build — the UUID Bazel prints.
    #[serde(deserialize_with = "invocation_id")]
    #[schemars(with = "String")]
    id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct EventsParams {
    /// BES invocation id of the build — the UUID Bazel prints.
    #[serde(deserialize_with = "invocation_id")]
    #[schemars(with = "String")]
    id: String,
    /// Only command, metadata, failure and finish events. Defaults to true.
    important: Option<bool>,
    /// Maximum records to return. Defaults to 100; values are clamped into
    /// [1, 500], not rejected.
    limit: Option<usize>,
    /// Records to skip, for paging. The response carries the unpaginated `total`.
    offset: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct BuildTargetsParams {
    /// BES invocation id of the build — the UUID Bazel prints.
    #[serde(deserialize_with = "invocation_id")]
    #[schemars(with = "String")]
    id: String,
    /// Maximum records to return. Defaults to 100; values are clamped into
    /// [1, 500], not rejected.
    limit: Option<usize>,
    /// Records to skip, for paging. The response carries the unpaginated `total`.
    offset: Option<usize>,
    /// label (default) or duration.
    sort: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct HistoryParams {
    /// Bazel label of the target or test, e.g. //app/backend:tests.
    label: String,
    /// Maximum records to return. Defaults to 100; values are clamped into
    /// [1, 500], not rejected.
    limit: Option<usize>,
    /// Records to skip, for paging. The response carries the unpaginated `total`.
    offset: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct TargetsParams {
    /// Maximum records to return. Defaults to 100; values are clamped into
    /// [1, 500], not rejected.
    limit: Option<usize>,
    /// Records to skip, for paging. The response carries the unpaginated `total`.
    offset: Option<usize>,
    /// label (default), mean_duration, max_duration, total_duration,
    /// executions or failures.
    sort: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct BlobParams {
    /// sha256 hash of the blob, from an action result digest.
    hash: String,
    /// Size of the blob in bytes, from the same digest.
    size: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct PrometheusParams {
    /// PromQL expression.
    query: String,
    /// Window ending now for a range query (30m, 6h, 7d, 2w); omit for an
    /// instant query.
    range: Option<String>,
    /// Distance between the points of a range query; default range/120.
    step: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct MetricsParams {
    /// Substring matched against every metrics line (help lines are always kept).
    filter: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListParams {
    /// Maximum records to return. Defaults to 100; values are clamped into
    /// [1, 500], not rejected.
    limit: Option<usize>,
    /// Records to skip, for paging. The response carries the unpaginated `total`.
    offset: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ExternalsParams {
    /// Maximum records to return. Defaults to 100; values are clamped into
    /// [1, 500], not rejected.
    limit: Option<usize>,
    /// Records to skip, for paging. The response carries the unpaginated `total`.
    offset: Option<usize>,
    /// Case-insensitive substring of the URL or the package name.
    q: Option<String>,
    /// python, npm, go, rust, java, bazel, github, docker, node or other.
    ecosystem: Option<String>,
    /// downloads (default), size, newest or name.
    sort: Option<String>,
}

/// Hostnames allowed in the `Host` header of `/mcp` requests.
///
/// The SDK defaults to loopback-only (DNS-rebinding protection), which blocks
/// access through a reverse proxy or ingress that preserves the original
/// host. `RBE_MCP_ALLOWED_HOSTS` extends the list with public names
/// (comma-separated); requests must still present a valid credential when
/// `RBE_AUTH_ENABLED` is on.
fn allowed_hosts_from_env() -> Option<Vec<String>> {
    let raw = std::env::var("RBE_MCP_ALLOWED_HOSTS").ok()?;
    let hosts: Vec<String> = raw
        .split(',')
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .collect();
    if hosts.is_empty() {
        None
    } else {
        Some(hosts)
    }
}

/// The streamable-HTTP config of `/mcp`: stateless JSON responses, and the
/// SDK's loopback `Host` allowlist (what `rbe-cli connect` sends through its
/// port-forward) plus `extra_hosts` (`RBE_MCP_ALLOWED_HOSTS`, the public
/// name). `with_allowed_hosts` would replace the loopback entries instead.
///
/// Any request carrying an `Origin` header is refused: MCP clients do not
/// send one, and browsers always do on a cross-origin POST, so a web page
/// cannot reach `/mcp` through the credentials `rbe-cli connect` injects.
fn server_config(extra_hosts: Option<Vec<String>>) -> StreamableHttpServerConfig {
    let mut config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .enforce_origin_validation();
    config.allowed_hosts.extend(extra_hosts.unwrap_or_default());
    config
}

/// The stateless streamable-HTTP router mounted at `/mcp`.
pub fn router(
    storage: BesStorage,
    cas: Option<SharedCasBackend>,
    workers: Option<Arc<WorkerRegistry>>,
    proxy: Option<Arc<PullThroughCache>>,
    metrics: Arc<Metrics>,
) -> axum::Router {
    let mcp = BesMcp::new(storage, cas, workers, proxy, metrics)
        .with_prometheus(PrometheusClient::from_env());
    let config = server_config(allowed_hosts_from_env());
    tracing::info!("MCP allowed hosts: {:?}", config.allowed_hosts);
    mcp_router(mcp, config)
}

fn mcp_router(mcp: BesMcp, config: StreamableHttpServerConfig) -> axum::Router {
    let service = StreamableHttpService::new(
        move || Ok(mcp.clone()),
        LocalSessionManager::default().into(),
        config,
    );
    axum::Router::new().nest_service("/mcp", service)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_hosts_env_parses_and_defaults_to_none() {
        std::env::remove_var("RBE_MCP_ALLOWED_HOSTS");
        assert!(allowed_hosts_from_env().is_none());

        std::env::set_var("RBE_MCP_ALLOWED_HOSTS", "rbe.example.com, localhost");
        let hosts = allowed_hosts_from_env().unwrap();
        assert_eq!(hosts, vec!["rbe.example.com", "localhost"]);
        std::env::remove_var("RBE_MCP_ALLOWED_HOSTS");
    }

    #[test]
    fn a_build_id_outside_the_data_directory_is_refused() {
        let params = |id: &str| serde_json::json!({ "id": id, "top": 5 });
        assert!(serde_json::from_value::<ProfileParams>(params("inv-1")).is_ok());
        for id in ["../etc", "/etc", "by-label"] {
            assert!(serde_json::from_value::<ProfileParams>(params(id)).is_err());
            assert!(
                serde_json::from_value::<BuildParams>(serde_json::json!({ "id": id })).is_err()
            );
        }
    }

    fn thousand() -> Vec<usize> {
        (0..1000).collect()
    }

    #[test]
    fn paginate_defaults_to_the_default_limit() {
        let page = paginate(thousand(), None, None);
        assert_eq!(page.total, 1000);
        assert_eq!(page.items.len(), DEFAULT_LIMIT);
    }

    #[test]
    fn paginate_clamps_oversized_limits_to_the_maximum() {
        let page = paginate(thousand(), Some(10_000), None);
        assert_eq!(page.items.len(), MAX_LIMIT);
        assert_eq!(page.items[0], 0);
    }

    #[test]
    fn paginate_clamps_a_zero_limit_to_one() {
        assert_eq!(paginate(thousand(), Some(0), None).items.len(), 1);
    }

    #[test]
    fn paginate_skips_to_the_offset() {
        let page = paginate(thousand(), Some(10), Some(15));
        assert_eq!(page.total, 1000);
        assert_eq!(page.items, (15..25).collect::<Vec<_>>());
    }

    #[test]
    fn paginate_past_the_end_is_empty_but_keeps_the_total() {
        let page = paginate(thousand(), Some(10), Some(2_000));
        assert_eq!(page.total, 1000);
        assert!(page.items.is_empty());
    }

    #[test]
    fn every_param_schema_rejects_unknown_fields() {
        for schema in [
            schemars::schema_for!(BuildParams),
            schemars::schema_for!(EventsParams),
            schemars::schema_for!(BuildTargetsParams),
            schemars::schema_for!(HistoryParams),
            schemars::schema_for!(TargetsParams),
            schemars::schema_for!(BlobParams),
            schemars::schema_for!(MetricsParams),
            schemars::schema_for!(ListParams),
            schemars::schema_for!(ProfileParams),
            schemars::schema_for!(ExternalsParams),
        ] {
            let json = serde_json::to_value(&schema).unwrap();
            assert_eq!(
                json["additionalProperties"],
                serde_json::json!(false),
                "params schema is not strict: {json}"
            );
            assert_eq!(json["type"], "object");
        }
    }

    #[tokio::test]
    async fn server_info_carries_agent_instructions() {
        let (_temp, mcp) = bes_mcp().await;
        let info = mcp.get_info();
        let instructions = info.instructions.expect("instructions set");
        for needle in ["invocation id", "total", "rbe_blob", "rbe_build_log"] {
            assert!(instructions.contains(needle), "missing {needle}");
        }
    }

    async fn bes_mcp() -> (tempfile::TempDir, BesMcp) {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = BesStorage::new(crate::bes::config::BesConfig {
            data_dir: temp.path().to_path_buf(),
            max_retention_days: None,
        })
        .await
        .unwrap();
        let mcp = BesMcp::new(storage, None, None, None, Arc::new(Metrics::new()));
        (temp, mcp)
    }

    #[tokio::test]
    async fn loopback_and_public_hosts_pass_and_any_browser_origin_is_refused() {
        use tower::ServiceExt;
        let (_temp, mcp) = bes_mcp().await;
        let router = mcp_router(
            mcp,
            server_config(Some(vec!["rbe.example.com".to_string()])),
        );
        let initialize = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;

        for (host, origin, allowed) in [
            ("rbe.example.com", None, true),
            ("localhost:18080", None, true),
            ("127.0.0.1:18080", None, true),
            ("attacker.example", None, false),
            ("localhost:18080", Some("https://attacker.example"), false),
            ("localhost:18080", Some("http://localhost:18080"), false),
            ("rbe.example.com", Some("https://rbe.example.com"), false),
        ] {
            let mut request = axum::http::Request::post("/mcp")
                .header("host", host)
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream");
            if let Some(origin) = origin {
                request = request.header("origin", origin);
            }
            let request = request.body(axum::body::Body::from(initialize)).unwrap();
            let status = router.clone().oneshot(request).await.unwrap().status();
            assert_eq!(status.is_success(), allowed, "{host} {origin:?}: {status}");
        }
    }

    #[tokio::test]
    async fn the_prometheus_tool_without_a_url_says_how_to_enable_it() {
        let (_temp, mcp) = bes_mcp().await;
        let error = mcp
            .rbe_prometheus(Parameters(PrometheusParams {
                query: "up".to_string(),
                range: None,
                step: None,
            }))
            .await
            .unwrap_err();
        assert!(error.message.contains("RBE_PROMETHEUS_URL"), "{error:?}");
    }

    #[tokio::test]
    async fn stats_tool_returns_json() {
        let (_temp, mcp) = bes_mcp().await;
        let result = mcp.rbe_stats().await.unwrap();
        let text = result.content[0].as_text().unwrap().text.clone();
        let stats: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(stats["total_builds"].is_i64());
    }

    #[tokio::test]
    async fn builds_tool_pages_with_total() {
        let (_temp, mcp) = bes_mcp().await;
        let result = mcp
            .rbe_builds(Parameters(ListParams {
                limit: None,
                offset: None,
            }))
            .await
            .unwrap();
        let text = result.content[0].as_text().unwrap().text.clone();
        let builds: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(builds["total"].is_u64());
        assert!(builds["items"].is_array());
    }

    #[tokio::test]
    async fn targets_tool_refuses_an_unknown_sort_and_histories_page() {
        let (_temp, mcp) = bes_mcp().await;
        let refused = mcp
            .rbe_targets(Parameters(TargetsParams {
                limit: None,
                offset: None,
                sort: Some("size".to_string()),
            }))
            .await;
        assert!(refused.is_err());
        let result = mcp
            .rbe_target_history(Parameters(HistoryParams {
                label: "//unknown:target".to_string(),
                limit: Some(10),
                offset: None,
            }))
            .await
            .unwrap();
        let text = result.content[0].as_text().unwrap().text.clone();
        let page: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(page, serde_json::json!({"total": 0, "items": []}));
    }

    #[tokio::test]
    async fn unknown_build_returns_the_storage_default_summary() {
        let (_temp, mcp) = bes_mcp().await;
        let result = mcp
            .rbe_build(Parameters(BuildParams {
                id: "nope".to_string(),
            }))
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
        let text = result.content[0].as_text().unwrap().text.clone();
        let build: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(build["invocation_id"], "nope");
    }

    #[test]
    fn a_profile_digest_keeps_the_longest_actions_and_totals_every_mnemonic() {
        let action = |name: &str, mnemonic: &str, duration_ms: f64| ProfileAction {
            name: name.to_string(),
            mnemonic: mnemonic.to_string(),
            duration_ms,
            ..Default::default()
        };
        let profile = BuildProfile {
            total_ms: 60_000.0,
            actions: vec![
                action("bundle a", "ViteBuild", 30_000.0),
                action("extract", "NpmPackageExtract", 20_000.0),
                action("bundle b", "ViteBuild", 5_000.0),
            ],
            critical_path: vec![
                CriticalStep {
                    name: "action 'extract'".to_string(),
                    start_ms: 0.0,
                    duration_ms: 20_000.0,
                },
                CriticalStep {
                    name: "action 'bundle a'".to_string(),
                    start_ms: 20_000.0,
                    duration_ms: 30_000.0,
                },
            ],
            ..Default::default()
        };

        let digest = ProfileDigest::new(profile, 2);

        assert_eq!(digest.critical_path_ms, 50_000.0);
        let longest: Vec<&str> = digest
            .longest_actions
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(longest, ["bundle a", "extract"]);
        assert_eq!(
            digest.mnemonics[0],
            MnemonicTotal {
                mnemonic: "ViteBuild".to_string(),
                actions: 2,
                total_ms: 35_000.0,
                max_ms: 30_000.0,
                executors: BTreeMap::from([(Executor::Unknown, 2)]),
            }
        );
    }

    #[tokio::test]
    async fn an_unknown_build_has_no_inputs() {
        let (_temp, mcp) = bes_mcp().await;
        let result = mcp
            .rbe_build_inputs(Parameters(BuildParams {
                id: "missing".to_string(),
            }))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn workers_tool_reports_an_empty_registry() {
        let (_temp, mcp) = bes_mcp().await;
        let result = mcp.rbe_workers().await.unwrap();
        let text = result.content[0].as_text().unwrap().text.clone();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            serde_json::json!([])
        );
    }

    async fn mcp_with_proxy(temp: &tempfile::TempDir) -> (tempfile::TempDir, BesMcp) {
        let storage = BesStorage::new(crate::bes::config::BesConfig {
            data_dir: temp.path().to_path_buf(),
            max_retention_days: None,
        })
        .await
        .unwrap();
        let proxy_temp = tempfile::TempDir::new().unwrap();
        let config = crate::proxy::ProxyConfig {
            dir: proxy_temp.path().to_path_buf(),
            ..Default::default()
        };
        let cache = PullThroughCache::new(config, Arc::new(Metrics::new()))
            .await
            .unwrap();
        let url = "https://example.com/tool.tar.gz";
        let key = crate::proxy::store::ProxyStore::cache_key(url);
        cache
            .index()
            .insert(
                key.clone(),
                Arc::new(crate::proxy::BlobMeta {
                    url: url.to_string(),
                    size: 5,
                    sha256_hex: "ab".repeat(32),
                    content_type: None,
                    etag: None,
                    fetched_at_unix: 0,
                    immutable: true,
                    downloads: 0,
                }),
            )
            .await;
        cache.index().record_download(&key);
        (
            proxy_temp,
            BesMcp::new(storage, None, None, Some(cache), Arc::new(Metrics::new())),
        )
    }

    #[tokio::test]
    async fn externals_tool_lists_cached_artifacts_with_counts() {
        let temp = tempfile::TempDir::new().unwrap();
        let (_proxy_temp, mcp) = mcp_with_proxy(&temp).await;
        let result = mcp
            .rbe_externals(Parameters(ExternalsParams {
                limit: Some(10),
                offset: None,
                q: None,
                ecosystem: None,
                sort: None,
            }))
            .await
            .unwrap();
        let text = result.content[0].as_text().unwrap().text.clone();
        let externals: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(externals["total"], 1);
        assert_eq!(
            externals["items"][0]["url"],
            "https://example.com/tool.tar.gz"
        );
        assert_eq!(externals["items"][0]["downloads"], 1);
        assert_eq!(externals["items"][0]["immutable"], true);
        assert_eq!(externals["items"][0]["artifact"]["ecosystem"], "other");
        assert_eq!(externals["ecosystems"][0]["artifacts"], 1);
    }

    #[tokio::test]
    async fn externals_tool_without_a_proxy_is_empty() {
        let (_temp, mcp) = bes_mcp().await;
        let result = mcp
            .rbe_externals(Parameters(ExternalsParams {
                limit: None,
                offset: None,
                q: None,
                ecosystem: None,
                sort: None,
            }))
            .await
            .unwrap();
        let text = result.content[0].as_text().unwrap().text.clone();
        let externals: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(externals["total"], 0);
        assert_eq!(externals["items"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn externals_tool_clamps_zero_limit_to_one() {
        let temp = tempfile::TempDir::new().unwrap();
        let (_proxy_temp, mcp) = mcp_with_proxy(&temp).await;
        let result = mcp
            .rbe_externals(Parameters(ExternalsParams {
                limit: Some(0),
                offset: None,
                q: None,
                ecosystem: None,
                sort: None,
            }))
            .await
            .unwrap();
        let text = result.content[0].as_text().unwrap().text.clone();
        let externals: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(externals["items"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn metrics_filter_keeps_matching_lines_and_help() {
        let (_temp, mcp) = bes_mcp().await;
        let result = mcp
            .rbe_metrics(Parameters(MetricsParams {
                filter: Some("rbe_cas_size_bytes".to_string()),
            }))
            .await
            .unwrap();
        let text = result.content[0].as_text().unwrap().text.clone();
        assert!(!text.contains("rbe_action_cache"));
        assert!(text.contains("rbe_cas_size_bytes") || text.contains("# HELP"));
    }

    #[tokio::test]
    async fn blob_tool_without_cas_is_an_internal_error() {
        let (_temp, mcp) = bes_mcp().await;
        let error = mcp
            .rbe_blob(Parameters(BlobParams {
                hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                    .to_string(),
                size: 0,
            }))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("CAS"));
    }
}
