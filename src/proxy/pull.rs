//! Orchestration of the pull-through path.
//!
//! Ties the index, the store, the upstream fetcher and request coalescing
//! together. This is the only module that knows the full lifecycle of a cached
//! artifact, and it is shared verbatim by the HTTP `/proxy/` routes and the
//! Remote Asset gRPC service so both see the same cache and the same coalescing.

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use reqwest::header::HeaderMap;
use tokio::fs::File;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::bes::models::Page;
use crate::bes::storage::MAX_PAGE_SIZE;
use crate::metrics::Metrics;
use crate::proxy::artifact::Ecosystem;
use crate::proxy::classifier::{self, AssetClass};
use crate::proxy::config::ProxyConfig;
use crate::proxy::error::{ProxyError, ProxyResult};
use crate::proxy::fetcher::Fetcher;
use crate::proxy::index::{build_meta, Lookup, ProxyEntrySnapshot, ProxyIndex};
use crate::proxy::singleflight::{self, Flight, FlightGuard, SingleFlight};
use crate::proxy::store::{BlobMeta, ProxyStore};

/// Channel depth for the live tee to the client. Bounded so a stalled client
/// applies backpressure instead of letting chunks pile up in memory.
const CLIENT_CHANNEL_DEPTH: usize = 16;

/// How an artifact is being delivered to the caller.
pub enum Served {
    /// The artifact is cached; read it straight off the PVC.
    FromDisk {
        meta: Arc<BlobMeta>,
        file: File,
        /// The request waited on another request's fetch of the same URL.
        coalesced: bool,
    },
    /// A cold fetch is under way; chunks are relayed as they arrive.
    Live {
        content_type: Option<String>,
        content_length: Option<u64>,
        chunks: mpsc::Receiver<ProxyResult<Bytes>>,
    },
}

/// The pull-through cache: index, on-disk store, upstream fetcher and
/// per-URL request coalescing behind one entry point per front door.
pub struct PullThroughCache {
    config: ProxyConfig,
    store: ProxyStore,
    index: Arc<ProxyIndex>,
    fetcher: Fetcher,
    flights: SingleFlight,
    metrics: Arc<Metrics>,
}

impl PullThroughCache {
    pub async fn new(config: ProxyConfig, metrics: Arc<Metrics>) -> ProxyResult<Arc<Self>> {
        let store = ProxyStore::new(&config).await?;
        let index = Arc::new(ProxyIndex::new(&config, store.clone()));
        let fetcher = Fetcher::new(&config)?;

        Ok(Arc::new(Self {
            config,
            store,
            index,
            fetcher,
            flights: SingleFlight::new(),
            metrics,
        }))
    }

    pub fn config(&self) -> &ProxyConfig {
        &self.config
    }

    pub fn index(&self) -> &Arc<ProxyIndex> {
        &self.index
    }

    /// Fold pending download deltas into the sidecars.
    pub async fn flush_downloads(&self) {
        self.index.flush_downloads().await;
    }

    /// Every cached artifact with its download count, most downloaded first.
    pub async fn list_externals(&self) -> Vec<ProxyEntrySnapshot> {
        self.index.snapshot().await
    }

    /// One page of [`Self::list_externals`]: the artifacts whose URL or
    /// package name contains `query` (case-insensitive) and, when given, that
    /// belong to `ecosystem`, in `sort` order. `limit` is clamped to
    /// `1..=MAX_PAGE_SIZE`. The per-ecosystem totals cover every artifact
    /// matching `query`, whatever the ecosystem filter, so a client can show
    /// them as the filter's options.
    ///
    /// The index is entirely in memory, so filtering the full snapshot per
    /// call is cheap even at tens of thousands of entries; the page bounds
    /// the response size, not the server-side work.
    pub async fn list_externals_page(
        &self,
        limit: usize,
        offset: usize,
        query: &str,
        ecosystem: Option<Ecosystem>,
        sort: ExternalsSort,
    ) -> ExternalsPage {
        let needle = query.to_lowercase();
        let matching: Vec<ProxyEntrySnapshot> = self
            .list_externals()
            .await
            .into_iter()
            .filter(|e| {
                needle.is_empty()
                    || e.url.to_lowercase().contains(&needle)
                    || e.artifact
                        .name
                        .as_deref()
                        .is_some_and(|n| n.to_lowercase().contains(&needle))
            })
            .collect();
        let ecosystems = ecosystem_totals(&matching);
        let mut items: Vec<ProxyEntrySnapshot> = matching
            .into_iter()
            .filter(|e| ecosystem.is_none_or(|wanted| e.artifact.ecosystem == wanted))
            .collect();
        sort.apply(&mut items);
        let total = items.len();
        let items = items
            .into_iter()
            .skip(offset)
            .take(limit.clamp(1, MAX_PAGE_SIZE))
            .collect();
        ExternalsPage {
            page: Page { items, total },
            ecosystems,
        }
    }

    /// Rebuild the in-memory index from the PVC and publish the disk metrics.
    /// See [`ProxyIndex::warm_up`].
    pub async fn warm_up(&self) -> ProxyResult<()> {
        self.index.warm_up().await?;
        self.publish_disk_metrics().await;
        Ok(())
    }

    /// Fetch `url`, serving from cache when possible.
    ///
    /// Concurrent requests for the same uncached URL share one upstream
    /// fetch: the first becomes the leader and streams live, the rest wait
    /// for its commit and read the blob from disk. Only the leader counts as
    /// a cache miss, so a herd of requests that costs one upstream fetch
    /// reports one miss and the rest as coalesced.
    pub async fn get(self: &Arc<Self>, url: &str) -> ProxyResult<Served> {
        let key = ProxyStore::cache_key(url);
        let class = classifier::classify(url);

        if let Some(served) = self.try_serve_cached(&key, class, false).await {
            return Ok(served);
        }

        match self.flights.begin(&key) {
            Flight::Leader(guard) => {
                self.metrics.record_proxy_miss(class.as_str());
                self.lead_fetch(url, class, key, guard).await
            }
            Flight::Follower(rx) => {
                self.metrics.record_proxy_coalesced();
                let meta = singleflight::wait(rx)
                    .await
                    .map_err(|e| ProxyError::Storage(e.to_string()))?;
                self.index.record_download(&key);
                self.open_committed(&key, meta, true).await
            }
        }
    }

    /// Resolve `url` to cached metadata without streaming it anywhere.
    ///
    /// This is the Remote Asset entry point: Bazel wants a digest, not bytes.
    ///
    /// `checksum_pinned` reports whether the caller supplied an expected content
    /// hash. That pin makes the asset immutable by definition regardless of what
    /// its URL looks like, which matters because plenty of `http_archive` URLs
    /// carry no recognisable extension and would otherwise be re-fetched on the
    /// mutable TTL forever.
    ///
    /// `headers` are not part of the cache key: a blob fetched once with
    /// credentials is served afterwards to anyone asking for the same URL.
    pub async fn resolve(
        self: &Arc<Self>,
        url: &str,
        checksum_pinned: bool,
        headers: &HeaderMap,
    ) -> ProxyResult<Arc<BlobMeta>> {
        let key = ProxyStore::cache_key(url);
        let class = if checksum_pinned {
            AssetClass::Immutable
        } else {
            classifier::classify(url)
        };

        if let Lookup::Fresh(meta) = self.index.get(&key).await {
            if self.store.open_blob(&key).await?.is_some() {
                self.metrics.record_proxy_hit(class.as_str());
                self.index.record_download(&key);
                return Ok(meta);
            }
            self.index.invalidate(&key).await;
        }

        match self.flights.begin(&key) {
            Flight::Leader(guard) => {
                self.metrics.record_proxy_miss(class.as_str());
                let outcome = self.fetch_to_disk(url, class, &key, None, headers).await;
                let meta = self.settle(guard, &key, outcome).await?;
                self.index.record_download(&key);
                Ok(meta)
            }
            Flight::Follower(rx) => {
                self.metrics.record_proxy_coalesced();
                let meta = singleflight::wait(rx)
                    .await
                    .map_err(|e| ProxyError::Storage(e.to_string()))?;
                self.index.record_download(&key);
                Ok(meta)
            }
        }
    }

    /// Absolute path of a cached blob, for hardlinking into the CAS.
    pub fn blob_path(&self, key: &str) -> std::path::PathBuf {
        self.store.blob_path(key)
    }

    /// Serve `key` from disk if it is indexed and fresh.
    ///
    /// An entry that is indexed but missing on disk is invalidated and
    /// reported as a miss, so the caller re-fetches it.
    async fn try_serve_cached(
        self: &Arc<Self>,
        key: &str,
        class: AssetClass,
        coalesced: bool,
    ) -> Option<Served> {
        let Lookup::Fresh(meta) = self.index.get(key).await else {
            return None;
        };

        match self.store.open_blob(key).await {
            Ok(Some(file)) => {
                self.metrics.record_proxy_hit(class.as_str());
                self.index.record_download(key);
                Some(Served::FromDisk {
                    meta,
                    file,
                    coalesced,
                })
            }
            Ok(None) => {
                warn!(key, "indexed blob missing on disk, invalidating");
                self.index.invalidate(key).await;
                None
            }
            Err(e) => {
                warn!(key, error = %e, "failed to open cached blob");
                None
            }
        }
    }

    /// Drive a cold fetch, teeing bytes to the client as they land on disk.
    ///
    /// The upstream handshake completes before this returns, so a 404 or a
    /// DNS failure reaches the client as a real status code rather than as a
    /// stream that dies mid-body. The body is then consumed on its own task:
    /// if the client hangs up the download still runs to completion, and the
    /// cache ends up with a whole file rather than a partial one.
    async fn lead_fetch(
        self: &Arc<Self>,
        url: &str,
        class: AssetClass,
        key: String,
        guard: FlightGuard,
    ) -> ProxyResult<Served> {
        let response = match self.fetcher.get(url, &HeaderMap::new()).await {
            Ok(r) => r,
            Err(e) => {
                guard.fail(e.to_string());
                return Err(e);
            }
        };

        let content_type = response.content_type.clone();
        let content_length = response.content_length;
        let (tx, rx) = mpsc::channel(CLIENT_CHANNEL_DEPTH);

        let this = self.clone();
        let url = url.to_string();
        self.index.record_download(&key);
        tokio::spawn(async move {
            let outcome = this
                .stream_body(&url, class, &key, response, Some(tx))
                .await;
            let _ = this.settle(guard, &key, outcome).await;
        });

        Ok(Served::Live {
            content_type,
            content_length,
            chunks: rx,
        })
    }

    /// Fetch and store without a client attached.
    async fn fetch_to_disk(
        self: &Arc<Self>,
        url: &str,
        class: AssetClass,
        key: &str,
        client: Option<mpsc::Sender<ProxyResult<Bytes>>>,
        headers: &HeaderMap,
    ) -> ProxyResult<Arc<BlobMeta>> {
        let response = self.fetcher.get(url, headers).await?;
        self.stream_body(url, class, key, response, client).await
    }

    /// Consume an upstream body, writing to disk and optionally to a client.
    ///
    /// The client is detached the first time a send fails (it hung up); the
    /// disk write always continues. A body larger than the cacheable limit,
    /// or a transport error, aborts the write and is relayed to a still
    /// attached client before being returned.
    async fn stream_body(
        self: &Arc<Self>,
        url: &str,
        class: AssetClass,
        key: &str,
        response: crate::proxy::fetcher::UpstreamResponse,
        client: Option<mpsc::Sender<ProxyResult<Bytes>>>,
    ) -> ProxyResult<Arc<BlobMeta>> {
        let mut writer = self.store.begin_write().await?;
        let mut body = response.body;
        let mut client_attached = client.is_some();

        while let Some(chunk) = body.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    if let (true, Some(tx)) = (client_attached, client.as_ref()) {
                        let _ = tx.send(Err(ProxyError::Storage(e.to_string()))).await;
                    }
                    return Err(e);
                }
            };

            if writer.size() + chunk.len() as u64 > self.config.max_blob_size_bytes {
                let err = ProxyError::BlobTooLarge {
                    size: writer.size() + chunk.len() as u64,
                };
                if let (true, Some(tx)) = (client_attached, client.as_ref()) {
                    let _ = tx.send(Err(ProxyError::BlobTooLarge { size: 0 })).await;
                }
                return Err(err);
            }

            writer.write_chunk(&chunk).await?;

            if client_attached {
                if let Some(tx) = client.as_ref() {
                    if tx.send(Ok(chunk)).await.is_err() {
                        debug!(url, "client disconnected; continuing download to disk");
                        client_attached = false;
                    }
                }
            }
        }

        let finished = writer.finish().await?;
        self.metrics.record_proxy_upstream_bytes(finished.size);

        let meta = build_meta(
            url,
            class,
            finished.sha256_hex.clone(),
            finished.size,
            response.content_type,
            response.etag,
        );
        self.store.commit(key, finished, &meta).await?;
        Ok(Arc::new(meta))
    }

    /// Publish a fetch outcome to followers and, on success, index the
    /// committed blob.
    async fn settle(
        &self,
        guard: FlightGuard,
        key: &str,
        outcome: ProxyResult<Arc<BlobMeta>>,
    ) -> ProxyResult<Arc<BlobMeta>> {
        match outcome {
            Ok(meta) => {
                self.index.insert(key.to_string(), meta.clone()).await;
                self.publish_disk_metrics().await;
                guard.resolve(meta.clone());
                Ok(meta)
            }
            Err(e) => {
                warn!(key, error = %e, "upstream fetch failed");
                guard.fail(e.to_string());
                Err(e)
            }
        }
    }

    /// Open the blob a leader just committed, for a coalesced follower.
    async fn open_committed(
        &self,
        key: &str,
        meta: Arc<BlobMeta>,
        coalesced: bool,
    ) -> ProxyResult<Served> {
        match self.store.open_blob(key).await? {
            Some(file) => Ok(Served::FromDisk {
                meta,
                file,
                coalesced,
            }),
            None => Err(ProxyError::Storage(
                "leader committed but the blob is gone".into(),
            )),
        }
    }

    async fn publish_disk_metrics(&self) {
        self.metrics
            .record_proxy_disk(self.index.disk_bytes(), self.index.entry_count().await);
    }
}

impl std::fmt::Debug for PullThroughCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PullThroughCache")
            .field("config", &self.config)
            .field("in_flight", &self.flights.len())
            .finish_non_exhaustive()
    }
}

/// Order of an externals listing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExternalsSort {
    /// Most served first.
    #[default]
    Downloads,
    /// Heaviest first.
    Size,
    /// Most recently fetched first.
    Newest,
    /// By package name, then version.
    Name,
}

impl ExternalsSort {
    /// The value the API accepts as `sort`; unknown values keep the default.
    pub fn parse(value: &str) -> Self {
        match value {
            "size" => Self::Size,
            "newest" => Self::Newest,
            "name" => Self::Name,
            _ => Self::Downloads,
        }
    }

    fn apply(self, items: &mut [ProxyEntrySnapshot]) {
        match self {
            Self::Downloads => {
                items.sort_by(|a, b| b.downloads.cmp(&a.downloads).then(a.url.cmp(&b.url)))
            }
            Self::Size => items.sort_by(|a, b| b.size.cmp(&a.size).then(a.url.cmp(&b.url))),
            Self::Newest => items.sort_by(|a, b| {
                b.fetched_at_unix
                    .cmp(&a.fetched_at_unix)
                    .then(a.url.cmp(&b.url))
            }),
            Self::Name => items.sort_by(|a, b| {
                a.artifact
                    .name
                    .cmp(&b.artifact.name)
                    .then(a.artifact.version.cmp(&b.artifact.version))
                    .then(a.url.cmp(&b.url))
            }),
        }
    }
}

/// Artifacts, bytes and serves of one ecosystem in the cache.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EcosystemTotal {
    pub ecosystem: Ecosystem,
    pub artifacts: usize,
    pub bytes: u64,
    pub downloads: u64,
    /// When the most recent of its artifacts was fetched, in Unix seconds.
    pub last_fetched_unix: u64,
}

/// A page of externals with the per-ecosystem totals of the whole listing.
#[derive(Debug, serde::Serialize)]
pub struct ExternalsPage {
    #[serde(flatten)]
    pub page: Page<ProxyEntrySnapshot>,
    pub ecosystems: Vec<EcosystemTotal>,
}

/// Per-ecosystem totals, the largest by artifacts first.
fn ecosystem_totals(items: &[ProxyEntrySnapshot]) -> Vec<EcosystemTotal> {
    let mut totals: std::collections::BTreeMap<Ecosystem, EcosystemTotal> =
        std::collections::BTreeMap::new();
    for item in items {
        let total = totals
            .entry(item.artifact.ecosystem)
            .or_insert(EcosystemTotal {
                ecosystem: item.artifact.ecosystem,
                artifacts: 0,
                bytes: 0,
                downloads: 0,
                last_fetched_unix: 0,
            });
        total.artifacts += 1;
        total.bytes += item.size;
        total.downloads += item.downloads;
        total.last_fetched_unix = total.last_fetched_unix.max(item.fetched_at_unix);
    }
    let mut totals: Vec<EcosystemTotal> = totals.into_values().collect();
    totals.sort_by(|a, b| b.artifacts.cmp(&a.artifacts));
    totals
}
