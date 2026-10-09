//! Upload of an action's declared outputs to the CAS.
//!
//! After the command exits, every declared output file is hashed and
//! uploaded, and every declared output directory is uploaded as an REAPI
//! `Tree`, yielding the metadata the `ActionResult` reports.
//!
//! Symlinks are never followed: a declared output that is a symlink is
//! reported as one, and a symlink inside an output directory becomes a
//! `SymlinkNode` of its `Tree`, both with the target exactly as on disk. That
//! is what Bazel expects by default (`--incompatible_remote_symlinks`), and it
//! keeps symlinks whose target only resolves at the destination, like the
//! `node_modules` rules_js lays out.
//!
//! Hashing streams each file through a fixed buffer. All the blobs the
//! outputs need are hashed first, the CAS is asked once which it lacks
//! (`FindMissingBlobs`), and only those are sent: files above
//! [`max_batch_size`] streamed from disk, the rest together in
//! `BatchUpdateBlobs` calls. An output with a thousand files costs a handful
//! of calls instead of two per file.

use bytes::Bytes;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use sha2::{Digest as Sha2Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::io::AsyncReadExt;
use tokio::sync::Semaphore;
use tokio_util::io::ReaderStream;
use tracing::{debug, error, info};

use crate::cas::{CasBackend, CasError, CasResult};
use crate::types::DigestInfo;

/// Environment variable overriding [`max_batch_size`], in bytes.
pub const ENV_MAX_BATCH_SIZE: &str = "RBE_MAX_BATCH_SIZE";

/// Largest file, in bytes, uploaded in a single write; larger files are
/// streamed. Default 4 MiB.
pub fn max_batch_size() -> i64 {
    std::env::var(ENV_MAX_BATCH_SIZE)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4 * 1024 * 1024)
}

type BoxedCasFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = CasResult<T>> + Send + 'a>>;

/// Whether any execute bit is set; always `false` off Unix.
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}

/// Tuning of [`OutputUploader`].
#[derive(Debug, Clone)]
pub struct UploaderConfig {
    /// Upper bound on uploads (batches or streamed blobs) at once. Default 10.
    pub max_concurrent_uploads: usize,
    /// Buffer size for hashing and streaming files, in bytes. Default 64 KiB.
    pub read_chunk_size: usize,
    /// Skip blobs the CAS already holds. Default on.
    pub skip_existing: bool,
}

impl Default for UploaderConfig {
    fn default() -> Self {
        Self {
            max_concurrent_uploads: 10,
            read_chunk_size: 64 * 1024,
            skip_existing: true,
        }
    }
}

/// An output file now in the CAS.
#[derive(Debug, Clone)]
pub struct UploadedFile {
    /// Path relative to the execroot.
    pub path: String,
    pub digest: DigestInfo,
    /// Size in bytes.
    pub size: i64,
    pub is_executable: bool,
}

/// An output directory now in the CAS.
#[derive(Debug, Clone)]
pub struct UploadedDirectory {
    /// Path relative to the execroot.
    pub path: String,
    /// Digest of the serialized REAPI `Tree` (not of its root `Directory`).
    pub tree_digest: DigestInfo,
    /// Files in the whole subtree.
    pub file_count: usize,
}

/// A declared output that is a symlink; nothing is uploaded for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedSymlink {
    /// Path relative to the execroot.
    pub path: String,
    /// The link's content exactly as `readlink` returns it.
    pub target: String,
}

/// Outcome of uploading an action's outputs. A failed output is recorded in
/// `errors` and does not stop the others.
#[derive(Debug, Default)]
pub struct UploadResult {
    pub files: Vec<UploadedFile>,
    pub directories: Vec<UploadedDirectory>,
    pub symlinks: Vec<UploadedSymlink>,
    pub bytes_uploaded: u64,
    /// Empty files, which need no upload.
    pub cached_files: usize,
    /// `(path, error)` for every output that could not be uploaded.
    pub errors: Vec<(String, String)>,
}

/// Uploads outputs from an execroot to the CAS.
pub struct OutputUploader {
    cas_backend: Arc<dyn CasBackend>,
    config: UploaderConfig,
    upload_semaphore: Arc<Semaphore>,
}

impl OutputUploader {
    pub fn new(cas_backend: Arc<dyn CasBackend>, config: UploaderConfig) -> Self {
        let upload_semaphore = Arc::new(Semaphore::new(config.max_concurrent_uploads));

        Self {
            cas_backend,
            config,
            upload_semaphore,
        }
    }

    /// Uploads the declared outputs, given relative to `root` (the command's
    /// working directory). An entry of `output_files` that turns out to be a
    /// directory is uploaded as one: with REAPI `output_paths` the client does
    /// not say which is which. An output the command did not produce is
    /// skipped, since actions may have optional outputs; one that cannot be
    /// read, or whose blobs fail to upload, is reported in
    /// [`UploadResult::errors`].
    ///
    /// Every output is hashed and every `Directory` and `Tree` built first;
    /// then one `FindMissingBlobs` asks the CAS which blobs it lacks, and only
    /// those are sent, small ones together in `BatchUpdateBlobs` calls of up
    /// to [`max_batch_size`] bytes and large ones streamed.
    pub async fn upload_outputs(
        &self,
        root: &Path,
        output_files: &[String],
        output_directories: &[String],
    ) -> UploadResult {
        debug!(
            "Uploading outputs: {} files, {} directories",
            output_files.len(),
            output_directories.len()
        );

        let mut result = UploadResult::default();
        let mut blobs = Blobs::default();
        let mut planned = Vec::new();
        let declared = output_files
            .iter()
            .map(|path| (path, false))
            .chain(output_directories.iter().map(|path| (path, true)));

        for (rel_path, declared_directory) in declared {
            match self
                .plan_output(root, rel_path, declared_directory, &mut blobs)
                .await
            {
                Ok(Some(output)) => planned.push(output),
                Ok(None) => debug!("Output {} was not produced", rel_path),
                Err(e) => {
                    error!("Failed to read output {}: {}", rel_path, e);
                    result.errors.push((rel_path.clone(), e.to_string()));
                }
            }
        }

        let failed = self.send_missing(&blobs).await;

        for output in planned {
            if let Some(e) = output.digests.iter().find_map(|d| failed.get(d)) {
                error!("Failed to upload output {}: {}", output.path(), e);
                result.errors.push((output.path().to_string(), e.clone()));
                continue;
            }
            match output.kind {
                PlannedKind::File(file) => {
                    if file.size == 0 {
                        result.cached_files += 1;
                    } else {
                        result.bytes_uploaded += file.size as u64;
                    }
                    result.files.push(file);
                }
                PlannedKind::Directory(directory) => result.directories.push(directory),
                PlannedKind::Symlink(symlink) => result.symlinks.push(symlink),
            }
        }

        info!(
            "Upload complete: {} files ({} cached), {} directories, {} symlinks, {} bytes uploaded, {} blobs checked",
            result.files.len(),
            result.cached_files,
            result.directories.len(),
            result.symlinks.len(),
            result.bytes_uploaded,
            blobs.len()
        );

        result
    }

    /// What one declared output is and the blobs it needs, or `None` when the
    /// command did not produce it.
    async fn plan_output(
        &self,
        root: &Path,
        rel_path: &str,
        declared_directory: bool,
        blobs: &mut Blobs,
    ) -> CasResult<Option<PlannedOutput>> {
        let full_path = root.join(rel_path);
        let metadata = match fs::symlink_metadata(&full_path).await {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(CasError::Io(e)),
        };

        let mut digests = Vec::new();
        let kind = if metadata.file_type().is_symlink() {
            PlannedKind::Symlink(UploadedSymlink {
                path: rel_path.to_string(),
                target: read_link_target(&full_path).await?,
            })
        } else if metadata.is_dir() {
            let (tree, file_count) = self.plan_tree(&full_path, blobs, &mut digests).await?;
            let tree_digest = DigestInfo::from_bytes(&tree);
            blobs.add(tree_digest, BlobSource::Bytes(Bytes::from(tree)));
            digests.push(tree_digest);
            PlannedKind::Directory(UploadedDirectory {
                path: rel_path.to_string(),
                tree_digest,
                file_count,
            })
        } else if declared_directory {
            return Err(CasError::InvalidData(
                "declared as an output directory but is a file".to_string(),
            ));
        } else {
            let size = metadata.len() as i64;
            let digest = self.hash_file(&full_path, size).await?;
            blobs.add(digest, BlobSource::File(full_path.clone()));
            digests.push(digest);
            PlannedKind::File(UploadedFile {
                path: rel_path.to_string(),
                digest,
                size,
                is_executable: is_executable(&metadata),
            })
        };
        Ok(Some(PlannedOutput { kind, digests }))
    }

    /// The serialized REAPI `Tree` of `dir_path` and its file count. `root`
    /// holds the top directory and `children` every descendant, flattened;
    /// every file and `Directory` is added to `blobs` and its digest to
    /// `digests`.
    async fn plan_tree(
        &self,
        dir_path: &Path,
        blobs: &mut Blobs,
        digests: &mut Vec<DigestInfo>,
    ) -> CasResult<(Vec<u8>, usize)> {
        use crate::proto::build::bazel::remote::execution::v2::Tree;

        let mut children = Vec::new();
        let (root, file_count) = self
            .plan_directory(dir_path, blobs, digests, &mut children)
            .await?;
        let tree = Tree {
            root: Some(root),
            children,
        };
        Ok((prost::Message::encode_to_vec(&tree), file_count))
    }

    /// The `Directory` of `dir_path` and the file count of its subtree,
    /// appending every descendant `Directory` to `children`.
    fn plan_directory<'a>(
        &'a self,
        dir_path: &'a Path,
        blobs: &'a mut Blobs,
        digests: &'a mut Vec<DigestInfo>,
        children: &'a mut Vec<crate::proto::build::bazel::remote::execution::v2::Directory>,
    ) -> BoxedCasFuture<
        'a,
        (
            crate::proto::build::bazel::remote::execution::v2::Directory,
            usize,
        ),
    > {
        Box::pin(async move {
            use crate::proto::build::bazel::remote::execution::v2::{
                Digest, Directory, DirectoryNode, FileNode, SymlinkNode,
            };
            let proto = |d: &DigestInfo| Digest {
                hash: d.hash_to_string(),
                size_bytes: d.size,
            };

            let mut dir = Directory::default();
            let mut file_count = 0;
            let mut entries = fs::read_dir(dir_path).await.map_err(CasError::Io)?;

            while let Some(entry) = entries.next_entry().await.map_err(CasError::Io)? {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                let file_type = entry.file_type().await.map_err(CasError::Io)?;

                if file_type.is_symlink() {
                    dir.symlinks.push(SymlinkNode {
                        name,
                        target: read_link_target(&path).await?,
                        node_properties: None,
                    });
                } else if file_type.is_file() {
                    let metadata = fs::metadata(&path).await.map_err(CasError::Io)?;
                    let digest = self.hash_file(&path, metadata.len() as i64).await?;
                    blobs.add(digest, BlobSource::File(path));
                    digests.push(digest);
                    dir.files.push(FileNode {
                        name,
                        digest: Some(proto(&digest)),
                        is_executable: is_executable(&metadata),
                        node_properties: None,
                    });
                    file_count += 1;
                } else if file_type.is_dir() {
                    let (subdir, subdir_count) =
                        self.plan_directory(&path, blobs, digests, children).await?;
                    let encoded = prost::Message::encode_to_vec(&subdir);
                    let subdir_digest = DigestInfo::from_bytes(&encoded);
                    blobs.add(subdir_digest, BlobSource::Bytes(Bytes::from(encoded)));
                    digests.push(subdir_digest);
                    children.push(subdir);
                    dir.directories.push(DirectoryNode {
                        name,
                        digest: Some(proto(&subdir_digest)),
                    });
                    file_count += subdir_count;
                }
            }

            dir.files.sort_by(|a, b| a.name.cmp(&b.name));
            dir.directories.sort_by(|a, b| a.name.cmp(&b.name));
            dir.symlinks.sort_by(|a, b| a.name.cmp(&b.name));
            Ok((dir, file_count))
        })
    }

    /// Sends the blobs the CAS lacks (all of them without
    /// [`UploaderConfig::skip_existing`]) and returns why each failed one did.
    async fn send_missing(&self, blobs: &Blobs) -> HashMap<DigestInfo, String> {
        let mut failed = HashMap::new();
        let all: Vec<DigestInfo> = blobs.sources.keys().copied().collect();
        let missing = if self.config.skip_existing {
            let mut missing = Vec::new();
            for chunk in all.chunks(FIND_MISSING_CHUNK) {
                match self.cas_backend.find_missing(chunk).await {
                    Ok(found) => missing.extend(found),
                    Err(e) => {
                        for digest in chunk {
                            failed.insert(*digest, format!("FindMissingBlobs failed: {e}"));
                        }
                    }
                }
            }
            missing
        } else {
            all
        };

        let limit = max_batch_size();
        let (large, small): (Vec<DigestInfo>, Vec<DigestInfo>) =
            missing.into_iter().partition(|d| d.size > limit);

        let mut sends = FuturesUnordered::new();
        for digest in large {
            sends.push(self.send_streamed(digest, &blobs.sources[&digest]));
        }
        for batch in batches_of(small, limit) {
            sends.push(self.send_batch(batch, blobs));
        }
        while let Some(outcome) = sends.next().await {
            failed.extend(outcome);
        }
        failed
    }

    /// Streams one large blob from its file; the failure, if any.
    fn send_streamed<'a>(&'a self, digest: DigestInfo, source: &'a BlobSource) -> BoxedSend<'a> {
        Box::pin(async move {
            let _permit = self.upload_semaphore.acquire().await;
            let sent = match source {
                BlobSource::File(path) => match tokio::fs::File::open(path).await {
                    Ok(file) => {
                        let stream = ReaderStream::with_capacity(file, self.config.read_chunk_size)
                            .map(|res| res.map_err(CasError::Io));
                        self.cas_backend
                            .write_stream(&digest, Box::pin(stream))
                            .await
                    }
                    Err(e) => Err(CasError::Io(e)),
                },
                BlobSource::Bytes(data) => self.cas_backend.write(&digest, data.clone()).await,
            };
            match sent {
                Ok(()) => {
                    info!(
                        "Streamed large output blob {} ({} bytes)",
                        digest, digest.size
                    );
                    Vec::new()
                }
                Err(e) => vec![(digest, e.to_string())],
            }
        })
    }

    /// Sends small blobs in one `BatchUpdateBlobs`; the failures, if any.
    fn send_batch<'a>(&'a self, batch: Vec<DigestInfo>, blobs: &'a Blobs) -> BoxedSend<'a> {
        Box::pin(async move {
            let _permit = self.upload_semaphore.acquire().await;
            let mut items = Vec::with_capacity(batch.len());
            let mut failed = Vec::new();
            for digest in batch {
                match &blobs.sources[&digest] {
                    BlobSource::Bytes(data) => items.push((digest, data.clone())),
                    BlobSource::File(path) => match fs::read(path).await {
                        Ok(data) => items.push((digest, Bytes::from(data))),
                        Err(e) => failed.push((digest, e.to_string())),
                    },
                }
            }
            match self.cas_backend.batch_write(&items).await {
                Ok(outcomes) => {
                    failed.extend(outcomes.into_iter().filter_map(|(digest, outcome)| {
                        outcome.err().map(|e| (digest, e.to_string()))
                    }))
                }
                Err(e) => failed.extend(items.iter().map(|(digest, _)| (*digest, e.to_string()))),
            }
            failed
        })
    }

    /// Stores the file at `path` in the CAS, streamed when it exceeds the
    /// batch limit and skipped when the CAS already holds it, and returns its
    /// digest.
    ///
    /// # Errors
    ///
    /// When the file cannot be read or the CAS refuses it.
    pub async fn upload_file(&self, path: &Path) -> CasResult<DigestInfo> {
        let size = fs::metadata(path).await.map_err(CasError::Io)?.len() as i64;
        let digest = self.hash_file(path, size).await?;
        let mut blobs = Blobs::default();
        blobs.add(digest, BlobSource::File(path.to_path_buf()));
        match self.send_missing(&blobs).await.remove(&digest) {
            None => Ok(digest),
            Some(reason) => Err(CasError::Storage(reason)),
        }
    }

    /// The digest of the file at `path`, hashed through a fixed buffer.
    async fn hash_file(&self, path: &Path, size: i64) -> CasResult<DigestInfo> {
        let mut file = tokio::fs::File::open(path).await.map_err(CasError::Io)?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; self.config.read_chunk_size];
        loop {
            let n = file.read(&mut buffer).await.map_err(CasError::Io)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
        DigestInfo::new(&hex::encode(hasher.finalize()), size)
            .map_err(|e| CasError::InvalidDigest(format!("Invalid file digest: {}", e)))
    }
}

/// Digests per `FindMissingBlobs` call.
const FIND_MISSING_CHUNK: usize = 10_000;

/// A send of one or more blobs and the `(digest, error)` of each that failed.
type BoxedSend<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Vec<(DigestInfo, String)>> + Send + 'a>>;

/// Where the content of a blob an output needs comes from.
enum BlobSource {
    /// A file of the execroot, read when it is sent.
    File(PathBuf),
    /// A `Directory` or `Tree` built here.
    Bytes(Bytes),
}

/// Every blob the outputs of one action need, each once.
#[derive(Default)]
struct Blobs {
    sources: HashMap<DigestInfo, BlobSource>,
}

impl Blobs {
    fn add(&mut self, digest: DigestInfo, source: BlobSource) {
        self.sources.entry(digest).or_insert(source);
    }

    fn len(&self) -> usize {
        self.sources.len()
    }
}

/// A declared output read from disk, not yet known to be in the CAS.
struct PlannedOutput {
    kind: PlannedKind,
    /// The blobs it needs; a failure on any of them fails the output.
    digests: Vec<DigestInfo>,
}

enum PlannedKind {
    File(UploadedFile),
    Directory(UploadedDirectory),
    Symlink(UploadedSymlink),
}

impl PlannedOutput {
    fn path(&self) -> &str {
        match &self.kind {
            PlannedKind::File(file) => &file.path,
            PlannedKind::Directory(directory) => &directory.path,
            PlannedKind::Symlink(symlink) => &symlink.path,
        }
    }
}

/// `digests` grouped into batches whose sizes add up to at most `limit`
/// bytes, each holding at least one.
fn batches_of(digests: Vec<DigestInfo>, limit: i64) -> Vec<Vec<DigestInfo>> {
    let mut batches = Vec::new();
    let mut batch = Vec::new();
    let mut bytes = 0;
    for digest in digests {
        if !batch.is_empty() && bytes + digest.size > limit {
            batches.push(std::mem::take(&mut batch));
            bytes = 0;
        }
        bytes += digest.size;
        batch.push(digest);
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    batches
}

/// The content of the symlink at `path`, which REAPI carries as a string.
async fn read_link_target(path: &Path) -> CasResult<String> {
    let target = fs::read_link(path).await.map_err(CasError::Io)?;
    target.into_os_string().into_string().map_err(|target| {
        CasError::InvalidData(format!(
            "symlink {} has a non-UTF-8 target {:?}",
            path.display(),
            target
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_skips_existing_blobs() {
        let config = UploaderConfig::default();
        assert_eq!(config.max_concurrent_uploads, 10);
        assert_eq!(config.read_chunk_size, 64 * 1024);
        assert!(config.skip_existing);
    }

    async fn uploader() -> (tempfile::TempDir, OutputUploader) {
        let cas_dir = tempfile::TempDir::new().unwrap();
        let cas = Arc::new(
            crate::cas::backends::DiskBackend::new(cas_dir.path())
                .await
                .unwrap(),
        );
        (cas_dir, OutputUploader::new(cas, UploaderConfig::default()))
    }

    /// A CAS that counts the calls the uploader makes.
    #[derive(Default)]
    struct Calls {
        find_missing: std::sync::atomic::AtomicUsize,
        batch_write: std::sync::atomic::AtomicUsize,
        write: std::sync::atomic::AtomicUsize,
        write_stream: std::sync::atomic::AtomicUsize,
        blobs_sent: std::sync::atomic::AtomicUsize,
    }

    struct CountingCas {
        inner: Arc<crate::cas::backends::DiskBackend>,
        calls: Arc<Calls>,
    }

    #[async_trait::async_trait]
    impl CasBackend for CountingCas {
        async fn contains(&self, digest: &DigestInfo) -> CasResult<bool> {
            self.inner.contains(digest).await
        }
        async fn read(&self, digest: &DigestInfo) -> CasResult<Option<Bytes>> {
            self.inner.read(digest).await
        }
        async fn read_stream(
            &self,
            digest: &DigestInfo,
            offset: usize,
            limit: Option<usize>,
        ) -> CasResult<Option<futures::stream::BoxStream<'static, CasResult<Bytes>>>> {
            self.inner.read_stream(digest, offset, limit).await
        }
        async fn write(&self, digest: &DigestInfo, data: Bytes) -> CasResult<()> {
            self.calls
                .write
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.calls
                .blobs_sent
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.write(digest, data).await
        }
        async fn write_stream(
            &self,
            digest: &DigestInfo,
            stream: futures::stream::BoxStream<'static, CasResult<Bytes>>,
        ) -> CasResult<()> {
            self.calls
                .write_stream
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.calls
                .blobs_sent
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.write_stream(digest, stream).await
        }
        async fn find_missing(&self, digests: &[DigestInfo]) -> CasResult<Vec<DigestInfo>> {
            self.calls
                .find_missing
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.find_missing(digests).await
        }
        async fn batch_write(
            &self,
            items: &[(DigestInfo, Bytes)],
        ) -> CasResult<Vec<(DigestInfo, CasResult<()>)>> {
            self.calls
                .batch_write
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.calls
                .blobs_sent
                .fetch_add(items.len(), std::sync::atomic::Ordering::Relaxed);
            self.inner.batch_write(items).await
        }
        async fn delete(&self, digest: &DigestInfo) -> CasResult<()> {
            self.inner.delete(digest).await
        }
        async fn local_path(&self, digest: &DigestInfo) -> CasResult<Option<PathBuf>> {
            self.inner.local_path(digest).await
        }
    }

    async fn counting_uploader() -> (tempfile::TempDir, OutputUploader, Arc<Calls>) {
        let cas_dir = tempfile::TempDir::new().unwrap();
        let inner = Arc::new(
            crate::cas::backends::DiskBackend::new(cas_dir.path())
                .await
                .unwrap(),
        );
        let calls = Arc::new(Calls::default());
        let cas = Arc::new(CountingCas {
            inner,
            calls: calls.clone(),
        });
        (
            cas_dir,
            OutputUploader::new(cas, UploaderConfig::default()),
            calls,
        )
    }

    fn count(counter: &std::sync::atomic::AtomicUsize) -> usize {
        counter.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[tokio::test]
    async fn a_directory_of_many_files_costs_one_lookup_and_a_few_batches() {
        let (_cas, uploader, calls) = counting_uploader().await;
        let root = tempfile::TempDir::new().unwrap();
        for d in 0..20 {
            std::fs::create_dir_all(root.path().join(format!("dist/d{d}"))).unwrap();
            for f in 0..10 {
                std::fs::write(
                    root.path().join(format!("dist/d{d}/f{f}.js")),
                    format!("export default {d}{f};"),
                )
                .unwrap();
            }
        }

        let result = uploader
            .upload_outputs(root.path(), &[], &["dist".to_string()])
            .await;

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.directories[0].file_count, 200);
        assert_eq!(count(&calls.find_missing), 1);
        assert_eq!(
            count(&calls.batch_write),
            1,
            "200 small files and 21 protos fit one batch"
        );
        assert_eq!(
            count(&calls.blobs_sent),
            200 + 20 + 1,
            "files, subdirectories and the tree, which carries the root"
        );
        assert_eq!(count(&calls.write) + count(&calls.write_stream), 0);
    }

    #[tokio::test]
    async fn outputs_already_in_the_cas_are_not_sent_again() {
        let (_cas, uploader, calls) = counting_uploader().await;
        let root = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join("dist")).unwrap();
        std::fs::write(root.path().join("dist/a.js"), b"a").unwrap();
        std::fs::write(root.path().join("out.txt"), b"out").unwrap();
        let outputs = ["out.txt".to_string(), "dist".to_string()];

        uploader.upload_outputs(root.path(), &outputs, &[]).await;
        let sent = count(&calls.blobs_sent);
        let again = uploader.upload_outputs(root.path(), &outputs, &[]).await;

        assert!(again.errors.is_empty(), "{:?}", again.errors);
        assert_eq!(
            count(&calls.blobs_sent),
            sent,
            "nothing is sent the second time"
        );
        assert_eq!(again.files.len() + again.directories.len(), 2);
    }

    #[tokio::test]
    async fn a_file_above_the_batch_limit_is_streamed() {
        let (_cas, uploader, calls) = counting_uploader().await;
        let root = tempfile::TempDir::new().unwrap();
        let large = vec![7u8; (max_batch_size() + 1) as usize];
        std::fs::write(root.path().join("big.bin"), &large).unwrap();
        std::fs::write(root.path().join("small.txt"), b"small").unwrap();

        let result = uploader
            .upload_outputs(
                root.path(),
                &["big.bin".to_string(), "small.txt".to_string()],
                &[],
            )
            .await;

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(count(&calls.write_stream), 1);
        assert_eq!(count(&calls.batch_write), 1);
        let big = result.files.iter().find(|f| f.path == "big.bin").unwrap();
        let stored = uploader
            .cas_backend
            .read(&big.digest)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.len(), large.len());
    }

    #[tokio::test]
    async fn output_paths_are_uploaded_as_files_or_directories_by_what_they_are() {
        let (_cas, uploader) = uploader().await;
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("out.txt"), b"file").unwrap();
        std::fs::create_dir_all(root.path().join("tree/sub")).unwrap();
        std::fs::write(root.path().join("tree/sub/leaf"), b"leaf").unwrap();

        let result = uploader
            .upload_outputs(
                root.path(),
                &["out.txt".to_string(), "tree".to_string()],
                &[],
            )
            .await;

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(
            result
                .files
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            vec!["out.txt"]
        );
        assert_eq!(
            result
                .directories
                .iter()
                .map(|d| d.path.as_str())
                .collect::<Vec<_>>(),
            vec!["tree"]
        );
    }

    #[tokio::test]
    async fn an_output_that_is_a_symlink_is_reported_as_one_even_if_dangling() {
        let (_cas, uploader) = uploader().await;
        let root = tempfile::TempDir::new().unwrap();
        std::os::unix::fs::symlink("../elsewhere/target", root.path().join("link")).unwrap();

        let result = uploader
            .upload_outputs(root.path(), &["link".to_string()], &[])
            .await;

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.files.is_empty() && result.directories.is_empty());
        assert_eq!(
            result.symlinks,
            vec![UploadedSymlink {
                path: "link".to_string(),
                target: "../elsewhere/target".to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn a_symlink_inside_an_output_directory_is_a_symlink_node_of_its_tree() {
        use crate::proto::build::bazel::remote::execution::v2::{SymlinkNode, Tree};
        let (_cas, uploader) = uploader().await;
        let root = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("tree")).unwrap();
        std::fs::write(root.path().join("tree/real"), b"content").unwrap();
        std::os::unix::fs::symlink("real", root.path().join("tree/link")).unwrap();

        let result = uploader
            .upload_outputs(root.path(), &["tree".to_string()], &[])
            .await;

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let tree_bytes = uploader
            .cas_backend
            .read(&result.directories[0].tree_digest)
            .await
            .unwrap()
            .unwrap();
        let tree_root = <Tree as prost::Message>::decode(tree_bytes)
            .unwrap()
            .root
            .unwrap();
        let file_names: Vec<_> = tree_root.files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            file_names,
            vec!["real"],
            "the link is not followed into a file"
        );
        assert_eq!(
            tree_root.symlinks,
            vec![SymlinkNode {
                name: "link".to_string(),
                target: "real".to_string(),
                node_properties: None,
            }]
        );
    }

    #[tokio::test]
    async fn an_output_that_was_not_produced_is_skipped_without_error() {
        let (_cas, uploader) = uploader().await;
        let root = tempfile::TempDir::new().unwrap();
        let result = uploader
            .upload_outputs(root.path(), &["missing".to_string()], &["gone".to_string()])
            .await;
        assert!(result.files.is_empty() && result.directories.is_empty());
        assert!(result.errors.is_empty(), "{:?}", result.errors);
    }

    #[tokio::test]
    async fn a_file_declared_as_an_output_directory_is_an_error() {
        let (_cas, uploader) = uploader().await;
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("dir"), b"not a dir").unwrap();
        let result = uploader
            .upload_outputs(root.path(), &[], &["dir".to_string()])
            .await;
        assert_eq!(result.errors.len(), 1);
    }
}
