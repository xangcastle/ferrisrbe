//! Request coalescing for concurrent fetches of the same artifact.
//!
//! A build that fans out across many CI runners asks for the same toolchain
//! tarball from all of them within the same second. Coalescing turns that
//! herd into a single upstream connection, which keeps the egress link and
//! the origin's rate limits out of the picture.
//!
//! Exactly one caller per key becomes the leader and performs the fetch. The
//! rest park on a [`watch`] channel and, once the leader commits, serve the
//! artifact from disk. Followers do not receive a live byte stream: relaying
//! chunks through a broadcast channel would hold them in memory for as long
//! as the slowest follower takes, defeating the streaming design.

use std::sync::Arc;

use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use tokio::sync::watch;

use crate::proxy::store::BlobMeta;

/// Resolution of an in-flight fetch.
#[derive(Clone, Debug)]
pub enum FetchState {
    /// The leader is still fetching.
    Pending,
    /// The artifact is committed to disk and readable.
    Done(Arc<BlobMeta>),
    /// The fetch failed. Carries a rendered message; `ProxyError` is not `Clone`.
    Failed(Arc<str>),
}

/// The role this caller was assigned for a given key.
#[derive(Debug)]
pub enum Flight {
    /// This caller must perform the fetch and resolve the guard.
    Leader(FlightGuard),
    /// Another caller is already fetching; await the result.
    Follower(watch::Receiver<FetchState>),
}

type FlightMap = Arc<DashMap<String, watch::Receiver<FetchState>>>;

/// Tracks which keys are currently being fetched.
#[derive(Debug, Clone, Default)]
pub struct SingleFlight {
    in_flight: FlightMap,
}

impl SingleFlight {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim leadership for `key`, or join the caller already holding it.
    ///
    /// Leader election is the atomicity of `DashMap::entry`: the vacant branch
    /// runs under the shard lock, so two callers cannot both become leader.
    pub fn begin(&self, key: &str) -> Flight {
        match self.in_flight.entry(key.to_string()) {
            Entry::Occupied(entry) => Flight::Follower(entry.get().clone()),
            Entry::Vacant(entry) => {
                let (tx, rx) = watch::channel(FetchState::Pending);
                entry.insert(rx);
                Flight::Leader(FlightGuard {
                    key: key.to_string(),
                    tx,
                    map: self.in_flight.clone(),
                    resolved: false,
                })
            }
        }
    }

    /// Number of fetches currently in flight.
    pub fn len(&self) -> usize {
        self.in_flight.len()
    }

    /// Whether no fetch is in flight.
    pub fn is_empty(&self) -> bool {
        self.in_flight.is_empty()
    }
}

/// Leadership token. Dropping it always releases the key, so a panicking or
/// early-returning leader cannot leave followers parked forever.
///
/// On drop the key is released before followers are notified, so a request
/// woken by the notification can already start a fresh fetch. A guard dropped
/// without [`Self::resolve`] or [`Self::fail`] publishes a failure.
#[derive(Debug)]
pub struct FlightGuard {
    key: String,
    tx: watch::Sender<FetchState>,
    map: FlightMap,
    resolved: bool,
}

impl FlightGuard {
    /// Publish a successful fetch to every follower.
    pub fn resolve(mut self, meta: Arc<BlobMeta>) {
        self.resolved = true;
        let _ = self.tx.send(FetchState::Done(meta));
    }

    /// Publish a failure to every follower.
    pub fn fail(mut self, message: impl Into<Arc<str>>) {
        self.resolved = true;
        let _ = self.tx.send(FetchState::Failed(message.into()));
    }
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        self.map.remove(&self.key);
        if !self.resolved {
            let _ = self
                .tx
                .send(FetchState::Failed(Arc::from("fetch abandoned by leader")));
        }
    }
}

/// Block until the leader resolves, then hand back the committed metadata.
///
/// A result published before the follower first polls is still observed,
/// because the watch channel retains its last value. When the sender is gone,
/// the retained value is authoritative: the guard publishes a failure before
/// dropping, so only a sender that vanished while still `Pending` yields the
/// "fetch leader vanished" error.
pub async fn wait(mut rx: watch::Receiver<FetchState>) -> Result<Arc<BlobMeta>, Arc<str>> {
    loop {
        if let Some(outcome) = settled(&rx.borrow_and_update()) {
            return outcome;
        }
        if rx.changed().await.is_err() {
            return settled(&rx.borrow())
                .unwrap_or_else(|| Err(Arc::from("fetch leader vanished")));
        }
    }
}

/// The outcome of a resolved fetch, or `None` while it is still pending.
fn settled(state: &FetchState) -> Option<Result<Arc<BlobMeta>, Arc<str>>> {
    match state {
        FetchState::Done(meta) => Some(Ok(meta.clone())),
        FetchState::Failed(err) => Some(Err(err.clone())),
        FetchState::Pending => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn meta(url: &str) -> Arc<BlobMeta> {
        Arc::new(BlobMeta {
            url: url.to_string(),
            size: 4,
            sha256_hex: "00".repeat(32),
            content_type: None,
            etag: None,
            fetched_at_unix: 0,
            immutable: true,
            downloads: 0,
        })
    }

    #[tokio::test]
    async fn first_caller_leads_and_second_follows() {
        let sf = SingleFlight::new();
        let leader = sf.begin("k");
        assert!(matches!(leader, Flight::Leader(_)));
        assert!(matches!(sf.begin("k"), Flight::Follower(_)));
        assert!(
            matches!(sf.begin("other"), Flight::Leader(_)),
            "distinct keys never contend"
        );
    }

    #[tokio::test]
    async fn follower_receives_the_leaders_result() {
        let sf = SingleFlight::new();
        let guard = match sf.begin("k") {
            Flight::Leader(g) => g,
            _ => panic!("expected leadership"),
        };
        let rx = match sf.begin("k") {
            Flight::Follower(rx) => rx,
            _ => panic!("expected to follow"),
        };

        let waiter = tokio::spawn(wait(rx));
        guard.resolve(meta("https://example.com/a"));

        let got = waiter.await.unwrap().unwrap();
        assert_eq!(got.url, "https://example.com/a");
    }

    #[tokio::test]
    async fn follower_sees_an_explicit_failure() {
        let sf = SingleFlight::new();
        let guard = match sf.begin("k") {
            Flight::Leader(g) => g,
            _ => panic!(),
        };
        let rx = match sf.begin("k") {
            Flight::Follower(rx) => rx,
            _ => panic!(),
        };

        let waiter = tokio::spawn(wait(rx));
        guard.fail("upstream 503");

        assert_eq!(&*waiter.await.unwrap().unwrap_err(), "upstream 503");
    }

    #[tokio::test]
    async fn dropping_the_guard_unblocks_followers_and_frees_the_key() {
        let sf = SingleFlight::new();
        let guard = match sf.begin("k") {
            Flight::Leader(g) => g,
            _ => panic!(),
        };
        let rx = match sf.begin("k") {
            Flight::Follower(rx) => rx,
            _ => panic!(),
        };

        let waiter = tokio::spawn(wait(rx));
        drop(guard);

        assert!(waiter.await.unwrap().is_err());
        assert!(sf.is_empty());
        assert!(
            matches!(sf.begin("k"), Flight::Leader(_)),
            "a later request must not inherit the abandoned fetch"
        );
    }

    #[tokio::test]
    async fn key_is_released_after_a_successful_resolve() {
        let sf = SingleFlight::new();
        if let Flight::Leader(g) = sf.begin("k") {
            g.resolve(meta("https://example.com/a"));
        }
        assert!(sf.is_empty());
        assert!(matches!(sf.begin("k"), Flight::Leader(_)));
    }

    #[tokio::test]
    async fn twenty_concurrent_callers_produce_exactly_one_fetch() {
        let sf = SingleFlight::new();
        let fetches = Arc::new(AtomicUsize::new(0));
        let coalesced = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..20 {
            let sf = sf.clone();
            let fetches = fetches.clone();
            let coalesced = coalesced.clone();
            tasks.push(tokio::spawn(async move {
                match sf.begin("hot-key") {
                    Flight::Leader(guard) => {
                        fetches.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        guard.resolve(meta("https://example.com/hot"));
                    }
                    Flight::Follower(rx) => {
                        coalesced.fetch_add(1, Ordering::SeqCst);
                        wait(rx).await.expect("follower should get the result");
                    }
                }
            }));
        }

        for t in tasks {
            t.await.unwrap();
        }

        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        assert_eq!(coalesced.load(Ordering::SeqCst), 19);
    }

    #[tokio::test]
    async fn waiting_on_an_already_resolved_channel_returns_immediately() {
        let sf = SingleFlight::new();
        let (guard, rx) = match (sf.begin("k"), sf.begin("k")) {
            (Flight::Leader(g), Flight::Follower(rx)) => (g, rx),
            _ => panic!(),
        };
        guard.resolve(meta("https://example.com/a"));

        let got = wait(rx).await.unwrap();
        assert_eq!(got.url, "https://example.com/a");
    }
}
