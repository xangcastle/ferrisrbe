//! Bazel's JSON trace profile (`command.profile.gz`, announced in
//! `BuildToolLogs` and uploaded to the CAS) reduced to what a Gantt chart of
//! a build needs: every action with its lane and times, where it ran, the
//! critical path and the phase markers.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::io::Read;

use serde::{Deserialize, Serialize};

use crate::bes::storage::BesStorage;
use crate::cache::action_cache_log::WorkerPlacements;
use crate::cas::SharedCasBackend;
use crate::types::DigestInfo;

/// Most actions a profile summary keeps: the longest ones, plus every action
/// on the critical path. A `//...` build runs tens of thousands of them.
pub const MAX_ACTIONS: usize = 4000;

/// The version of [`summarize`]'s output. A saved summary of an older one is
/// summarized again from the CAS when it still holds the profile.
pub const SUMMARY_VERSION: u32 = 3;

/// Largest profile [`summarize`] decompresses. An uncached `//...` with
/// `--noslim_profile` writes about 205 MB (17 MB gzipped).
const MAX_PROFILE_BYTES: u64 = 1 << 30;

/// A build's profile as the UI draws it. Times are milliseconds from the
/// start of the Bazel command.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BuildProfile {
    /// [`SUMMARY_VERSION`] when summarized; `0` for summaries saved before it.
    #[serde(default)]
    pub version: u32,
    pub total_ms: f64,
    pub phases: Vec<ProfilePhase>,
    /// Number of distinct lanes (Bazel threads) the actions ran on.
    pub lanes: u32,
    /// Actions, longest first, at most [`MAX_ACTIONS`].
    pub actions: Vec<ProfileAction>,
    /// Actions the summary left out for being short.
    pub omitted_actions: usize,
    /// The critical path, in order.
    pub critical_path: Vec<CriticalStep>,
    /// Action count and time per executor, over every action of the
    /// profile, the omitted ones included.
    #[serde(default)]
    pub executors: BTreeMap<Executor, ExecutorTotal>,
    /// Action count and time per RBE worker, over every remote action whose
    /// primary output the server saw that worker produce.
    #[serde(default)]
    pub workers: BTreeMap<String, ExecutorTotal>,
}

/// Where an action ran, from the events Bazel profiled on its thread while
/// the action was processed.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Executor {
    /// Summarized before executors were recorded.
    #[default]
    Unknown,
    /// Executed by the RBE workers: `execute remotely`.
    Remote,
    /// Executed in a sandbox on the machine that ran Bazel: `sandbox.*`,
    /// `subprocess.run`.
    Sandbox,
    /// Executed unsandboxed on the machine that ran Bazel: `local action
    /// execution`.
    Local,
    /// Served by the remote or disk cache: a `check cache hit` and no
    /// execution.
    Cache,
    /// Spawned no process (file writes, symlinks, template expansions).
    Internal,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ExecutorTotal {
    pub actions: usize,
    pub total_ms: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProfilePhase {
    pub name: String,
    pub start_ms: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProfileAction {
    pub name: String,
    pub mnemonic: String,
    /// The target that owns the action, with
    /// `--experimental_profile_include_target_label`.
    pub target: Option<String>,
    /// The primary output, with `--experimental_profile_include_primary_output`.
    pub output: Option<String>,
    pub start_ms: f64,
    pub duration_ms: f64,
    pub lane: u32,
    pub critical: bool,
    #[serde(default)]
    pub executor: Executor,
    /// The RBE worker that ran a remote action, matched by its primary output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CriticalStep {
    pub name: String,
    pub start_ms: f64,
    pub duration_ms: f64,
}

#[derive(Deserialize)]
struct Trace {
    #[serde(rename = "traceEvents")]
    events: Vec<TraceEvent>,
}

#[derive(Deserialize)]
struct TraceEvent {
    #[serde(default)]
    cat: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    ts: f64,
    #[serde(default)]
    dur: f64,
    #[serde(default)]
    tid: i64,
    /// The primary output: Bazel writes it beside `args`, not inside.
    #[serde(default)]
    out: Option<String>,
    #[serde(default)]
    args: Option<TraceArgs>,
}

#[derive(Deserialize, Default)]
struct TraceArgs {
    #[serde(default)]
    mnemonic: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default, rename = "out")]
    output: Option<String>,
}

/// Where a build's profile summary stands.
pub enum ProfileLookup {
    Found(BuildProfile),
    /// The build announced no profile.
    NotAnnounced,
    /// The CAS no longer holds the profile the build announced.
    Evicted,
    /// The profile blob is not a Bazel JSON profile.
    Unreadable(anyhow::Error),
}

/// The saved summary of a build's profile, or one summarized from the CAS
/// and saved for the next read.
pub async fn load(
    storage: &BesStorage,
    cas: &SharedCasBackend,
    invocation_id: &str,
) -> anyhow::Result<ProfileLookup> {
    let saved = match storage.get_saved_profile(invocation_id).await {
        Ok(saved) => saved,
        Err(e) => {
            tracing::error!(
                "Failed to read the saved profile of {}: {}",
                invocation_id,
                e
            );
            None
        }
    };
    if let Some(profile) = &saved {
        if profile.version >= SUMMARY_VERSION {
            return Ok(ProfileLookup::Found(saved.unwrap_or_default()));
        }
    }
    let stale_or = |lookup: ProfileLookup| match saved.clone() {
        Some(profile) => ProfileLookup::Found(profile),
        None => lookup,
    };
    let Some((hash, size)) = storage.get_profile_blob(invocation_id).await? else {
        return Ok(stale_or(ProfileLookup::NotAnnounced));
    };
    let Ok(digest) = DigestInfo::new(&hash, size) else {
        return Ok(stale_or(ProfileLookup::NotAnnounced));
    };
    let Some(bytes) = cas.read(&digest).await? else {
        return Ok(stale_or(ProfileLookup::Evicted));
    };
    let placements = storage.get_placements(invocation_id).await;
    let profile = match tokio::task::spawn_blocking(move || summarize(&bytes, &placements)).await? {
        Ok(profile) => profile,
        Err(e) => return Ok(stale_or(ProfileLookup::Unreadable(e))),
    };
    if let Err(e) = storage.save_profile(invocation_id, &profile).await {
        tracing::error!("Failed to save the profile of {}: {}", invocation_id, e);
    }
    Ok(ProfileLookup::Found(profile))
}

/// Summarizes a gzipped (or plain) Bazel JSON profile; `placements` names
/// the worker of each remote action.
pub fn summarize(bytes: &[u8], placements: &WorkerPlacements) -> anyhow::Result<BuildProfile> {
    let json: Cow<[u8]> = if bytes.starts_with(&[0x1f, 0x8b]) {
        Cow::Owned(gunzip_at_most(bytes, MAX_PROFILE_BYTES)?)
    } else {
        Cow::Borrowed(bytes)
    };
    let trace: Trace = serde_json::from_slice(&json)?;
    Ok(summarize_events(trace.events, placements))
}

/// Decompresses `gzipped`, refusing output past `limit` bytes so a small
/// upload cannot expand into all of the server's memory.
fn gunzip_at_most(gzipped: &[u8], limit: u64) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(gzipped)
        .take(limit + 1)
        .read_to_end(&mut out)?;
    anyhow::ensure!(
        out.len() as u64 <= limit,
        "the profile decompresses to more than {limit} bytes"
    );
    Ok(out)
}

/// What an event on an action's thread says about where the action ran.
fn execution_signal(cat: Option<&str>, name: &str) -> Option<Executor> {
    match (cat, name) {
        (Some("remote action execution"), "execute remotely") => Some(Executor::Remote),
        (Some("local action execution"), _) => Some(Executor::Local),
        (_, "subprocess.run") => Some(Executor::Sandbox),
        (_, name) if name.starts_with("sandbox.") => Some(Executor::Sandbox),
        (Some("remote action cache check"), "check cache hit") => Some(Executor::Cache),
        _ => None,
    }
}

/// The executor of an action from the signals on its thread during its
/// span. An execution outranks the cache check that precedes it; a remote
/// execution outranks the local fallback around it.
fn executor_of(signals: &[(f64, Executor)], start_us: f64, end_us: f64) -> Executor {
    let from = signals.partition_point(|(ts, _)| *ts < start_us);
    let rank = |e: Executor| match e {
        Executor::Remote => 4,
        Executor::Sandbox => 3,
        Executor::Local => 2,
        Executor::Cache => 1,
        Executor::Unknown | Executor::Internal => 0,
    };
    signals[from..]
        .iter()
        .take_while(|(ts, _)| *ts <= end_us)
        .map(|(_, e)| *e)
        .max_by_key(|e| rank(*e))
        .unwrap_or(Executor::Internal)
}

fn summarize_events(events: Vec<TraceEvent>, placements: &WorkerPlacements) -> BuildProfile {
    let ms = |us: f64| us / 1000.0;
    let mut phases = Vec::new();
    let mut critical_path = Vec::new();
    let mut actions = Vec::new();
    let mut spans = Vec::new();
    let mut signals: HashMap<i64, Vec<(f64, Executor)>> = HashMap::new();
    let mut lanes: HashMap<i64, u32> = HashMap::new();
    let mut end_us: f64 = 0.0;

    for event in events {
        end_us = end_us.max(event.ts + event.dur);
        if let Some(executor) = execution_signal(event.cat.as_deref(), &event.name) {
            signals
                .entry(event.tid)
                .or_default()
                .push((event.ts, executor));
            continue;
        }
        match event.cat.as_deref() {
            Some("build phase marker") if event.ts >= 0.0 => phases.push(ProfilePhase {
                name: event.name,
                start_ms: ms(event.ts),
            }),
            Some("critical path component") => critical_path.push(CriticalStep {
                name: event.name,
                start_ms: ms(event.ts),
                duration_ms: ms(event.dur),
            }),
            Some("action processing") => {
                let next = lanes.len() as u32;
                let lane = *lanes.entry(event.tid).or_insert(next);
                spans.push((event.tid, event.ts, event.ts + event.dur));
                let args = event.args.unwrap_or_default();
                actions.push(ProfileAction {
                    name: event.name,
                    mnemonic: args.mnemonic.unwrap_or_default(),
                    target: args.target,
                    output: event.out.or(args.output),
                    start_ms: ms(event.ts),
                    duration_ms: ms(event.dur),
                    lane,
                    critical: false,
                    executor: Executor::Unknown,
                    worker: None,
                });
            }
            _ => {}
        }
    }

    for thread in signals.values_mut() {
        thread.sort_by(|a, b| a.0.total_cmp(&b.0));
    }
    let mut executors: BTreeMap<Executor, ExecutorTotal> = BTreeMap::new();
    let mut workers: BTreeMap<String, ExecutorTotal> = BTreeMap::new();
    for (action, (tid, start, end)) in actions.iter_mut().zip(&spans) {
        action.executor = signals.get(tid).map_or(Executor::Internal, |thread| {
            executor_of(thread, *start, *end)
        });
        let total = executors.entry(action.executor).or_default();
        total.actions += 1;
        total.total_ms += action.duration_ms;
        if action.executor == Executor::Remote {
            action.worker = action
                .output
                .as_deref()
                .and_then(|output| placements.worker_of(output))
                .map(str::to_string);
        }
        if let Some(worker) = &action.worker {
            let total = workers.entry(worker.clone()).or_default();
            total.actions += 1;
            total.total_ms += action.duration_ms;
        }
    }

    for step in &critical_path {
        let Some(name) = step
            .name
            .strip_prefix("action '")
            .and_then(|n| n.strip_suffix('\''))
        else {
            continue;
        };
        if let Some(action) = actions
            .iter_mut()
            .find(|a| a.name == name && (a.start_ms - step.start_ms).abs() < 1.0 + step.duration_ms)
        {
            action.critical = true;
        }
    }

    actions.sort_by(|a, b| {
        b.critical
            .cmp(&a.critical)
            .then(b.duration_ms.total_cmp(&a.duration_ms))
    });
    let omitted_actions = actions.len().saturating_sub(MAX_ACTIONS);
    actions.truncate(MAX_ACTIONS);
    phases.sort_by(|a, b| a.start_ms.total_cmp(&b.start_ms));
    critical_path.sort_by(|a, b| a.start_ms.total_cmp(&b.start_ms));

    BuildProfile {
        version: SUMMARY_VERSION,
        total_ms: ms(end_us),
        phases,
        lanes: lanes.len() as u32,
        actions,
        omitted_actions,
        critical_path,
        executors,
        workers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const PROFILE: &str = r#"{"otherData":{},"traceEvents":[
        {"cat":"build phase marker","name":"Launch Blaze","ph":"X","ts":-37000,"dur":37000,"tid":1},
        {"cat":"build phase marker","name":"Initialize command","ph":"i","ts":0,"tid":1},
        {"cat":"build phase marker","name":"Execute","ph":"i","ts":400000,"tid":1},
        {"cat":"action processing","name":"Extracting npm package prettier@3.8.1","ph":"X","ts":640260,"dur":9796636,"tid":7,"args":{"mnemonic":"NpmPackageExtract","target":"//:node_modules/prettier"}},
        {"cat":"action processing","name":"Writing file a.txt","ph":"X","ts":650000,"dur":1000,"tid":8,"args":{"mnemonic":"FileWrite"}},
        {"cat":"action processing","name":"Writing file b.txt","ph":"X","ts":652000,"dur":500,"tid":7,"args":{"mnemonic":"FileWrite"}},
        {"cat":"critical path component","name":"action 'Extracting npm package prettier@3.8.1'","ph":"X","ts":640229,"dur":9796609,"tid":0},
        {"cat":"general information","name":"x","ph":"X","ts":19000000,"dur":200000,"tid":1}
    ]}"#;

    #[test]
    fn a_profile_becomes_lanes_actions_and_the_critical_path() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(PROFILE.as_bytes()).unwrap();
        let profile = summarize(&gz.finish().unwrap(), &WorkerPlacements::default()).unwrap();

        assert_eq!(profile.total_ms, 19200.0);
        assert_eq!(
            profile
                .phases
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Initialize command", "Execute"]
        );
        assert_eq!(profile.lanes, 2);
        assert_eq!(profile.actions.len(), 3);
        let first = &profile.actions[0];
        assert!(first.critical);
        assert_eq!(first.mnemonic, "NpmPackageExtract");
        assert_eq!(first.target.as_deref(), Some("//:node_modules/prettier"));
        assert_eq!(first.lane, 0);
        assert_eq!(profile.actions[1].lane, 1);
        assert!(!profile.actions[1].critical);
        assert_eq!(profile.critical_path.len(), 1);
        assert_eq!(profile.omitted_actions, 0);
    }

    #[test]
    fn each_action_is_attributed_to_where_it_ran() {
        let profile = summarize(
            br#"{"traceEvents":[
            {"cat":"action processing","name":"remote","ph":"X","ts":1000,"dur":5000,"tid":1,"args":{"mnemonic":"ViteBuild"}},
            {"cat":"remote action cache check","name":"check cache hit","ph":"X","ts":1100,"dur":100,"tid":1},
            {"cat":"remote action execution","name":"execute remotely","ph":"X","ts":1300,"dur":4000,"tid":1},
            {"cat":"action processing","name":"sandboxed","ph":"X","ts":1000,"dur":3000,"tid":2,"args":{"mnemonic":"Genrule"}},
            {"cat":"general information","name":"sandbox.createFileSystem","ph":"X","ts":1200,"dur":10,"tid":2},
            {"cat":"general information","name":"subprocess.run","ph":"X","ts":1300,"dur":2000,"tid":2},
            {"cat":"action processing","name":"unsandboxed","ph":"X","ts":1000,"dur":2000,"tid":3,"args":{"mnemonic":"Genrule"}},
            {"cat":"local action execution","name":"Genrule","ph":"X","ts":1100,"dur":1800,"tid":3},
            {"cat":"action processing","name":"cached","ph":"X","ts":5000,"dur":500,"tid":3,"args":{"mnemonic":"TsProject"}},
            {"cat":"remote action cache check","name":"check cache hit","ph":"X","ts":5100,"dur":300,"tid":3},
            {"cat":"action processing","name":"written","ph":"X","ts":9000,"dur":100,"tid":2,"args":{"mnemonic":"FileWrite"}}
        ]}"#,
            &WorkerPlacements::default(),
        )
        .unwrap();

        let executor = |name: &str| {
            profile
                .actions
                .iter()
                .find(|a| a.name == name)
                .unwrap()
                .executor
        };
        assert_eq!(executor("remote"), Executor::Remote);
        assert_eq!(executor("sandboxed"), Executor::Sandbox);
        assert_eq!(executor("unsandboxed"), Executor::Local);
        assert_eq!(executor("cached"), Executor::Cache);
        assert_eq!(executor("written"), Executor::Internal);
        assert_eq!(profile.version, SUMMARY_VERSION);
        assert_eq!(profile.executors[&Executor::Remote].actions, 1);
        assert_eq!(profile.executors[&Executor::Remote].total_ms, 5.0);
        assert_eq!(profile.executors.len(), 5);
    }

    #[tokio::test]
    async fn a_remote_action_names_the_worker_that_produced_its_primary_output() {
        let log = crate::cache::action_cache_log::ActionCacheLog::new();
        log.record_placement(
            "inv",
            "rbe-worker-7f",
            ["bazel-out/bin/app/dist", "bazel-out/bin/app/x.map"],
        )
        .await;
        let placements = log.placements("inv").await;
        let profile = summarize(
            br#"{"traceEvents":[
            {"cat":"action processing","name":"ViteBuild //app","ph":"X","ts":1000,"dur":5000,"tid":1,"out":"bazel-out/bin/app/dist","args":{"mnemonic":"ViteBuild","target":"//app"}},
            {"cat":"remote action execution","name":"execute remotely","ph":"X","ts":1300,"dur":4000,"tid":1},
            {"cat":"action processing","name":"local","ph":"X","ts":1000,"dur":2000,"tid":2,"args":{"mnemonic":"Genrule","out":"bazel-out/bin/app/x.map"}},
            {"cat":"local action execution","name":"Genrule","ph":"X","ts":1100,"dur":1800,"tid":2}
        ]}"#,
            &placements,
        )
        .unwrap();

        let worker = |name: &str| {
            profile
                .actions
                .iter()
                .find(|a| a.name == name)
                .unwrap()
                .worker
                .clone()
        };
        assert_eq!(worker("ViteBuild //app").as_deref(), Some("rbe-worker-7f"));
        assert_eq!(worker("local"), None);
        assert_eq!(profile.workers["rbe-worker-7f"].actions, 1);
        assert_eq!(profile.workers.len(), 1);
    }

    #[test]
    fn a_summary_saved_before_executors_reads_as_unknown() {
        let old = r#"{"total_ms":1.0,"phases":[],"lanes":1,"actions":[{"name":"a","mnemonic":"M","start_ms":0.0,"duration_ms":1.0,"lane":0,"critical":false}],"omitted_actions":0,"critical_path":[]}"#;
        let profile: BuildProfile = serde_json::from_str(old).unwrap();
        assert_eq!(profile.version, 0);
        assert_eq!(profile.actions[0].executor, Executor::Unknown);
    }

    #[test]
    fn a_plain_json_profile_is_read_too() {
        assert_eq!(
            summarize(PROFILE.as_bytes(), &WorkerPlacements::default())
                .unwrap()
                .actions
                .len(),
            3
        );
    }

    #[test]
    fn decompression_stops_at_the_limit() {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&[b' '; 4096]).unwrap();
        let gzipped = encoder.finish().unwrap();

        assert_eq!(gunzip_at_most(&gzipped, 4096).unwrap().len(), 4096);
        assert!(gunzip_at_most(&gzipped, 4095).is_err());
    }
}
