//! On-disk layout shared by the disk CAS and the disk Action Cache.
//!
//! The entry for SHA-256 `aabbcc…` lives at `<root>/aa/bb/cc…`: two levels of
//! 256-way fan-out keep every directory small with millions of entries.
//! Names starting with `.` (the `.tmp` staging directory) are never entries.
//! Writes stage into `.tmp` and are published with a rename, so a reader
//! never observes a partially written entry.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use tracing::{info, warn};

use crate::cache::lru::LruState;
use crate::types::DigestInfo;

/// Name of the staging directory under the store root.
pub const STAGING_DIR: &str = ".tmp";

/// Entries folded into an [`LruState`] per lock hold while restoring it.
const MERGE_CHUNK: usize = 10_000;

/// An entry found on disk.
#[derive(Debug, Clone, Copy)]
pub struct ScannedEntry {
    /// The digest named by the path, with size `0`: the layout encodes only
    /// the hash.
    pub digest: DigestInfo,
    /// Size of the file, in bytes.
    pub file_size: u64,
    /// Last modification time, in milliseconds since the Unix epoch.
    pub mtime_millis: u64,
}

/// What [`restore_recency`] added to an [`LruState`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RestoredTotals {
    pub entries: u64,
    pub bytes: u64,
}

/// Path of the entry for `digest` under `root`.
pub fn entry_path(root: &Path, digest: &DigestInfo) -> PathBuf {
    let hash = digest.hash_to_string();
    root.join(&hash[0..2]).join(&hash[2..4]).join(&hash[4..])
}

/// Staging files this old are leftovers of a process that died mid-write:
/// a live write touches its file with every chunk.
pub const STALE_STAGING_AGE: Duration = Duration::from_secs(60 * 60);

/// A fresh, unique path in `staging_dir` for a write about to be published.
pub fn staging_path(staging_dir: &Path) -> PathBuf {
    staging_dir.join(format!("tmp-{}", uuid::Uuid::new_v4()))
}

/// A path in the staging directory that is deleted when dropped, so a write
/// that fails or whose task is cancelled before it publishes leaves nothing
/// behind. The removal is synchronous; it runs only on those paths.
pub struct StagingFile {
    path: PathBuf,
    armed: bool,
}

impl StagingFile {
    pub fn new(staging_dir: &Path) -> Self {
        Self {
            path: staging_path(staging_dir),
            armed: true,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Hands the file over to the caller, who has published or removed it.
    pub fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for StagingFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// [`sweep_staging`] of the files older than [`STALE_STAGING_AGE`], logged;
/// a failure is only logged, since leftovers never affect what is served.
pub async fn sweep_staging_logged(staging_dir: &Path) {
    match sweep_staging(staging_dir.to_path_buf(), STALE_STAGING_AGE).await {
        Ok((0, _)) => {}
        Ok((files, bytes)) => info!(
            "Removed {} stale staging files ({} bytes) from {:?}",
            files, bytes, staging_dir
        ),
        Err(e) => warn!("Failed to sweep {:?}: {}", staging_dir, e),
    }
}

/// Removes the files of `staging_dir` last modified more than `older_than`
/// ago; returns how many and their bytes. Younger files may belong to a write
/// in flight, of this process or of the one it replaces on the same volume.
pub async fn sweep_staging(
    staging_dir: PathBuf,
    older_than: Duration,
) -> std::io::Result<(u64, u64)> {
    tokio::task::spawn_blocking(move || {
        let (mut files, mut bytes) = (0, 0);
        for entry in std::fs::read_dir(&staging_dir)? {
            let entry = entry?;
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let stale = metadata
                .modified()
                .ok()
                .and_then(|at| at.elapsed().ok())
                .is_some_and(|age| age > older_than);
            if metadata.is_file() && stale && std::fs::remove_file(entry.path()).is_ok() {
                files += 1;
                bytes += metadata.len();
            }
        }
        Ok((files, bytes))
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Creates the parent directories of `path`.
pub async fn create_parent_dirs(path: &Path) -> std::io::Result<()> {
    match path.parent() {
        Some(parent) => tokio::fs::create_dir_all(parent).await,
        None => Ok(()),
    }
}

/// The digest named by `path`, relative to `root`, with size `0`.
///
/// Accepts the sharded form `aa/bb/cc…` and a flat 64-hex-digit file name;
/// anything else is not an entry.
pub fn digest_from_path(root: &Path, path: &Path) -> Option<DigestInfo> {
    let components: Vec<&str> = path
        .strip_prefix(root)
        .ok()?
        .components()
        .map(|c| c.as_os_str().to_str())
        .collect::<Option<_>>()?;
    let hash = match components.as_slice() {
        [flat] => (*flat).to_string(),
        [a, b, rest] => format!("{a}{b}{rest}"),
        _ => return None,
    };
    if hash.len() != 64 {
        return None;
    }
    DigestInfo::new(&hash, 0).ok()
}

/// Every entry under `root`, walked on the blocking-task pool.
///
/// The walk issues one directory read and one stat per entry, which for a
/// large store is hundreds of thousands of syscalls; running it as a single
/// blocking task keeps the async worker threads free to serve requests while
/// it runs. A file that disappears mid-walk (evicted, expired, replaced) is
/// omitted.
pub async fn scan(root: PathBuf) -> std::io::Result<Vec<ScannedEntry>> {
    tokio::task::spawn_blocking(move || {
        let mut found = Vec::new();
        scan_dir_blocking(&root, &root, &mut found)?;
        Ok(found)
    })
    .await
    .map_err(std::io::Error::other)?
}

fn scan_dir_blocking(root: &Path, dir: &Path, out: &mut Vec<ScannedEntry>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let hidden = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name.starts_with('.'));
        if hidden {
            continue;
        }
        if path.is_dir() {
            scan_dir_blocking(root, &path, out)?;
            continue;
        }
        let Some(digest) = digest_from_path(root, &path) else {
            continue;
        };
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if metadata.is_file() {
            out.push(ScannedEntry {
                digest,
                file_size: metadata.len(),
                mtime_millis: mtime_millis(&metadata),
            });
        }
    }
    Ok(())
}

/// Folds scanned entries into `lru`, ranked by mtime below every live touch.
///
/// Entries whose content hash `lru` already tracks were accounted by a live
/// write while the scan ran and are skipped. The lock is released between
/// chunks so concurrent requests are never blocked for the whole restore.
///
/// Entries are folded oldest first, each at the later of its mtime and the
/// slot after the previous one. Many blobs share a millisecond, and handing
/// their stamps to [`LruState::insert_at`] unsorted makes it probe the whole
/// run of occupied counters for each one: quadratic in the entries written
/// in a burst.
pub async fn restore_recency(
    lru: &Mutex<LruState>,
    entries: impl IntoIterator<Item = (DigestInfo, u64, u64)>,
) -> RestoredTotals {
    let mut entries: Vec<_> = entries.into_iter().collect();
    entries.sort_unstable_by_key(|&(_, _, stamp)| stamp);
    let mut totals = RestoredTotals::default();
    let mut next_free = 0;
    for chunk in entries.chunks(MERGE_CHUNK) {
        {
            let mut lru = lru.lock();
            for &(digest, size, stamp) in chunk {
                let stamp = stamp.max(next_free);
                if lru.insert_at(digest, size, stamp) {
                    totals.entries += 1;
                    totals.bytes += size;
                    next_free = stamp + 1;
                }
            }
        }
        tokio::task::yield_now().await;
    }
    totals
}

/// Milliseconds since the Unix epoch, the recency scale of restored entries.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn mtime_millis(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .unwrap_or(UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_path_round_trips_through_digest_from_path() {
        let root = Path::new("/store");
        let digest = DigestInfo::from_bytes(b"payload");
        let path = entry_path(root, &digest);
        let parsed = digest_from_path(root, &path).unwrap();
        assert_eq!(parsed.hash, digest.hash);
        assert_eq!(parsed.size, 0);
    }

    #[test]
    fn digest_from_path_rejects_non_entries() {
        let root = Path::new("/store");
        assert!(digest_from_path(root, Path::new("/store/ab/cd")).is_none());
        assert!(digest_from_path(root, Path::new("/elsewhere/ab/cd/ef")).is_none());
        assert!(digest_from_path(root, Path::new("/store/ab/cd/not-hex")).is_none());
    }

    #[tokio::test]
    async fn restore_ranks_by_mtime_whatever_the_scan_order() {
        let lru = Mutex::new(LruState::new());
        let newer = DigestInfo::from_bytes(b"newer");
        let same_ms = DigestInfo::from_bytes(b"same ms");
        let older = DigestInfo::from_bytes(b"older");
        let totals =
            restore_recency(&lru, [(newer, 1, 20), (same_ms, 2, 20), (older, 3, 10)]).await;
        assert_eq!((totals.entries, totals.bytes), (3, 6));
        let mut lru = lru.lock();
        assert_eq!(lru.pop_lru(), Some((older, 3)));
        let rest = [lru.pop_lru().unwrap().0, lru.pop_lru().unwrap().0];
        assert!(rest.contains(&newer) && rest.contains(&same_ms));
    }

    #[tokio::test]
    async fn restoring_a_burst_written_in_one_millisecond_is_linear() {
        let lru = Mutex::new(LruState::new());
        let burst = 200_000u32;
        let entries = (0..burst).map(|i| (DigestInfo::from_bytes(&i.to_le_bytes()), 1, 1_000));
        let started = std::time::Instant::now();
        let totals = restore_recency(&lru, entries).await;
        assert_eq!(totals.entries, u64::from(burst));
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn a_staging_file_is_removed_on_drop_unless_disarmed() {
        let dir = tempfile::TempDir::new().unwrap();
        let dropped = StagingFile::new(dir.path());
        std::fs::write(dropped.path(), b"partial").unwrap();
        let dropped_path = dropped.path().to_path_buf();
        drop(dropped);
        assert!(!dropped_path.exists());

        let kept = StagingFile::new(dir.path());
        std::fs::write(kept.path(), b"published").unwrap();
        let kept_path = kept.path().to_path_buf();
        kept.disarm();
        assert!(kept_path.exists());
    }

    #[tokio::test]
    async fn the_sweep_removes_only_stale_staging_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let stale = dir.path().join("tmp-stale");
        let live = dir.path().join("tmp-live");
        std::fs::write(&stale, b"left by a dead process").unwrap();
        std::fs::write(&live, b"in flight").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(SystemTime::now() - 2 * STALE_STAGING_AGE)
            .unwrap();

        let swept = sweep_staging(dir.path().to_path_buf(), STALE_STAGING_AGE)
            .await
            .unwrap();
        assert_eq!(swept, (1, 22));
        assert!(!stale.exists());
        assert!(live.exists());
    }
}
