//! Per-invocation record of Action Cache lookups, keyed by the `target_id`
//! Bazel sends in `RequestMetadata`. The BES joins it with the build's
//! targets to tell which ones actually executed, and with its profile to
//! tell which worker ran each remote action.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use moka::future::Cache;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// How long an invocation's record is kept after it was created.
const INVOCATION_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Invocations retained at once; the least recently used is dropped first.
const MAX_INVOCATIONS: u64 = 2_000;
/// Invocations whose worker placements are retained at once. Only builds
/// with remote execution record any; a `//...` one is about a megabyte.
const MAX_PLACED_INVOCATIONS: u64 = 200;
/// Outputs placed per invocation; later ones are not recorded.
const MAX_PLACED_OUTPUTS: usize = 1_000_000;

/// Stable key of an output's exec path (FNV-1a, 64 bits): placements are
/// persisted, so the key must not change across builds of the server.
pub fn output_key(path: &str) -> u64 {
    path.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Which worker produced each output of an invocation's remote executions.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerPlacements {
    pub workers: Vec<String>,
    /// `(output_key of the exec path, index into workers)`, sorted by key.
    pub outputs: Vec<(u64, u32)>,
}

impl WorkerPlacements {
    /// The worker that produced the output at `path`, if any.
    pub fn worker_of(&self, path: &str) -> Option<&str> {
        let key = output_key(path);
        let at = self.outputs.binary_search_by_key(&key, |(k, _)| *k).ok()?;
        self.workers
            .get(self.outputs[at].1 as usize)
            .map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.outputs.is_empty()
    }
}

#[derive(Debug, Default)]
struct PlacementsBuilder {
    workers: Vec<String>,
    worker_index: HashMap<String, u32>,
    outputs: HashMap<u64, u32>,
}

/// The inputs of a target's heaviest remotely executed action: what a
/// worker had to materialize before running it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionInputs {
    pub mnemonic: String,
    pub files: u64,
    pub bytes: u64,
    /// Heaviest groups: npm packages, external repositories, directories.
    #[serde(default)]
    pub groups: Vec<InputGroupStats>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputGroupStats {
    pub name: String,
    pub files: u64,
    pub bytes: u64,
}

/// Action Cache outcome of one target's actions within an invocation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TargetCacheStats {
    pub hits: u64,
    pub misses: u64,
    /// Mnemonic of every action that missed, with how many times it did.
    #[serde(default)]
    pub missed_mnemonics: BTreeMap<String, u64>,
    /// The target's heaviest remotely executed action, when one ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heaviest_inputs: Option<ActionInputs>,
}

/// Bounded, time-limited log of Action Cache lookups per invocation.
#[derive(Debug)]
pub struct ActionCacheLog {
    invocations: Cache<String, Arc<DashMap<String, TargetCacheStats>>>,
    placements: Cache<String, Arc<Mutex<PlacementsBuilder>>>,
}

impl Default for ActionCacheLog {
    fn default() -> Self {
        Self::new()
    }
}

impl ActionCacheLog {
    pub fn new() -> Self {
        Self {
            invocations: Cache::builder()
                .max_capacity(MAX_INVOCATIONS)
                .time_to_live(INVOCATION_TTL)
                .build(),
            placements: Cache::builder()
                .max_capacity(MAX_PLACED_INVOCATIONS)
                .time_to_live(INVOCATION_TTL)
                .build(),
        }
    }

    /// Records that `worker` produced `outputs` (exec paths) in
    /// `invocation_id`.
    pub async fn record_placement<'a>(
        &self,
        invocation_id: &str,
        worker: &str,
        outputs: impl IntoIterator<Item = &'a str>,
    ) {
        let placements = self
            .placements
            .get_with(invocation_id.to_string(), async {
                Arc::new(Mutex::new(PlacementsBuilder::default()))
            })
            .await;
        let mut placements = placements.lock();
        let next = placements.workers.len() as u32;
        let index = *placements
            .worker_index
            .entry(worker.to_string())
            .or_insert(next);
        if index == next {
            placements.workers.push(worker.to_string());
        }
        for output in outputs {
            if placements.outputs.len() >= MAX_PLACED_OUTPUTS {
                break;
            }
            placements.outputs.insert(output_key(output), index);
        }
    }

    /// The worker placements recorded for an invocation, empty if none.
    pub async fn placements(&self, invocation_id: &str) -> WorkerPlacements {
        let Some(placements) = self.placements.get(invocation_id).await else {
            return WorkerPlacements::default();
        };
        let placements = placements.lock();
        let mut outputs: Vec<(u64, u32)> = placements
            .outputs
            .iter()
            .map(|(key, worker)| (*key, *worker))
            .collect();
        outputs.sort_unstable();
        WorkerPlacements {
            workers: placements.workers.clone(),
            outputs,
        }
    }

    /// Counts one lookup by an action of `target` in `invocation_id`.
    pub async fn record(&self, invocation_id: &str, target: &str, mnemonic: &str, hit: bool) {
        let targets = self
            .invocations
            .get_with(invocation_id.to_string(), async {
                Arc::new(DashMap::new())
            })
            .await;
        let mut stats = targets.entry(target.to_string()).or_default();
        if hit {
            stats.hits += 1;
        } else {
            stats.misses += 1;
            *stats
                .missed_mnemonics
                .entry(mnemonic.to_string())
                .or_default() += 1;
        }
    }

    /// Keeps `inputs` as the heaviest of `target` in `invocation_id` when no
    /// action of it recorded more bytes.
    pub async fn record_inputs(&self, invocation_id: &str, target: &str, inputs: ActionInputs) {
        let targets = self
            .invocations
            .get_with(invocation_id.to_string(), async {
                Arc::new(DashMap::new())
            })
            .await;
        let mut stats = targets.entry(target.to_string()).or_default();
        if stats
            .heaviest_inputs
            .as_ref()
            .is_none_or(|known| inputs.bytes > known.bytes)
        {
            stats.heaviest_inputs = Some(inputs);
        }
    }

    /// Snapshot of an invocation's lookups, sorted by target label.
    pub async fn snapshot(&self, invocation_id: &str) -> BTreeMap<String, TargetCacheStats> {
        match self.invocations.get(invocation_id).await {
            Some(targets) => targets
                .iter()
                .map(|e| (e.key().clone(), e.value().clone()))
                .collect(),
            None => BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod placement_tests {
    use super::*;

    #[test]
    fn the_output_key_is_fnv1a() {
        assert_eq!(output_key(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(output_key("a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[tokio::test]
    async fn each_output_maps_to_the_worker_that_produced_it() {
        let log = ActionCacheLog::new();
        log.record_placement(
            "inv",
            "rbe-worker-a",
            ["bazel-out/bin/a.js", "bazel-out/bin/a.map"],
        )
        .await;
        log.record_placement("inv", "rbe-worker-b", ["bazel-out/bin/b.js"])
            .await;
        log.record_placement("inv", "rbe-worker-a", ["bazel-out/bin/c.js"])
            .await;

        let placements = log.placements("inv").await;
        assert_eq!(placements.workers, vec!["rbe-worker-a", "rbe-worker-b"]);
        assert_eq!(
            placements.worker_of("bazel-out/bin/a.map"),
            Some("rbe-worker-a")
        );
        assert_eq!(
            placements.worker_of("bazel-out/bin/b.js"),
            Some("rbe-worker-b")
        );
        assert_eq!(
            placements.worker_of("bazel-out/bin/c.js"),
            Some("rbe-worker-a")
        );
        assert_eq!(placements.worker_of("bazel-out/bin/d.js"), None);
        assert!(log.placements("other").await.is_empty());

        let json = serde_json::to_string(&placements).unwrap();
        assert_eq!(
            serde_json::from_str::<WorkerPlacements>(&json).unwrap(),
            placements
        );
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn a_target_keeps_the_inputs_of_its_heaviest_action() {
        let log = ActionCacheLog::new();
        let inputs = |mnemonic: &str, bytes| ActionInputs {
            mnemonic: mnemonic.to_string(),
            files: 10,
            bytes,
            groups: vec![],
        };
        log.record_inputs("inv", "//app:web", inputs("TsProject", 10))
            .await;
        log.record_inputs("inv", "//app:web", inputs("ViteBuild", 500))
            .await;
        log.record_inputs("inv", "//app:web", inputs("CopyFile", 20))
            .await;
        log.record("inv", "//app:web", "ViteBuild", false).await;

        let stats = log.snapshot("inv").await;
        let heaviest = stats["//app:web"].heaviest_inputs.as_ref().unwrap();
        assert_eq!(
            (heaviest.mnemonic.as_str(), heaviest.bytes),
            ("ViteBuild", 500)
        );
        assert_eq!(stats["//app:web"].misses, 1);
    }

    use super::*;

    #[tokio::test]
    async fn counts_hits_and_misses_per_target_and_mnemonic() {
        let log = ActionCacheLog::new();
        log.record("inv", "//a:lib", "GoCompilePkg", false).await;
        log.record("inv", "//a:lib", "GoCompilePkg", false).await;
        log.record("inv", "//a:lib", "GoLink", true).await;
        log.record("inv", "//b:bin", "Rustc", true).await;
        log.record("other", "//a:lib", "Rustc", false).await;

        let snap = log.snapshot("inv").await;
        assert_eq!(snap.len(), 2);
        let a = &snap["//a:lib"];
        assert_eq!((a.hits, a.misses), (1, 2));
        assert_eq!(a.missed_mnemonics["GoCompilePkg"], 2);
        assert_eq!(snap["//b:bin"].misses, 0);
        assert!(log.snapshot("unknown").await.is_empty());
    }
}
