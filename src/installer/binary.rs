use std::collections::BTreeMap;
use std::fs::File;
use std::future::Future;
use std::io;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use futures::StreamExt;
use serde_json::to_vec_pretty;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio::fs;
use zip::ZipArchive;

use crate::installer::cache::{
    AGENTS_DIR, BinaryCacheLock, BinaryCacheMetadata, BinaryCachePaths, EXTRACTED_DIR_NAME,
    METADATA_FILE_NAME, acquire_binary_cache_lock, acquire_binary_cache_use_read_lock,
    acquire_binary_cache_use_write_lock, binary_cache_paths_with_digest, cache_root_dir,
    platform_cache_key, safe_path_component, try_acquire_binary_cache_lock,
};
use crate::registry::{BinaryTarget, Platform, RegistryAgent};

/// Name of the human-readable install log written into the cache root.
///
/// The image ships without a shell, so this file is how install state and
/// failures can be inspected from a container:
/// `docker cp <container>:/cache/acp-agent/agent-install.log .`
const INSTALL_LOG_FILE_NAME: &str = "agent-install.log";
/// Upper bound for the install log; it is append-only and lives in a cache
/// volume that may persist for a long time.
const INSTALL_LOG_MAX_BYTES: u64 = 1024 * 1024;
/// When the cap is hit, the log is rewritten to keep only this tail.
const INSTALL_LOG_TAIL_BYTES: u64 = 256 * 1024;

/// Hard resource limits applied to binary downloads and archive extraction.
#[derive(Debug, Clone, Copy)]
pub struct ArchiveLimits {
    /// Maximum compressed response bytes accepted from the network.
    pub max_download_bytes: u64,
    /// Maximum total uncompressed entry bytes written from an archive.
    pub max_expanded_bytes: u64,
    /// Maximum archive entries, including directories and symlinks.
    pub max_entries: u64,
    /// Maximum non-directory entries created from an archive.
    pub max_files: u64,
    /// Maximum time spent establishing the HTTP connection.
    pub connect_timeout: Duration,
    /// Maximum idle interval while reading the HTTP response.
    pub read_timeout: Duration,
    /// Maximum end-to-end HTTP request duration.
    pub total_timeout: Duration,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            max_download_bytes: 256 * 1024 * 1024,
            max_expanded_bytes: 512 * 1024 * 1024,
            max_entries: 10_000,
            max_files: 5_000,
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(30),
            total_timeout: Duration::from_secs(10 * 60),
        }
    }
}

/// A validated binary distribution stored in the local cache.
#[derive(Debug, Clone)]
pub struct CachedBinary {
    /// Resolved executable path within the extracted payload.
    pub executable_path: PathBuf,
    /// Directory containing the extracted payload.
    pub extracted_dir: PathBuf,
    /// Stable cache directory that owns the extracted payload.
    pub cache_dir: PathBuf,
    /// Shared payload-use lease held by runners and served routes.
    #[doc(hidden)]
    pub(crate) cache_use_lease: Option<Arc<BinaryCacheLock>>,
}

impl PartialEq for CachedBinary {
    fn eq(&self, other: &Self) -> bool {
        self.executable_path == other.executable_path
            && self.extracted_dir == other.extracted_dir
            && self.cache_dir == other.cache_dir
    }
}

impl Eq for CachedBinary {}

/// Ensures the current binary target exists in the stable local cache.
///
/// Every attempt (cache hit, fresh install, or failure) is appended to the
/// install log inside the cache root so that installs can be audited and
/// failures diagnosed even in images without a shell.
pub async fn cache_binary_target(
    agent: &RegistryAgent,
    platform: Platform,
    target: &BinaryTarget,
) -> Result<CachedBinary> {
    let result = match cache_root_dir() {
        Ok(root_dir) => cache_binary_target_in(&root_dir, agent, platform, target).await,
        Err(error) => Err(error),
    };
    record_install_log(agent, platform, &result).await;
    result
}

pub(crate) async fn cache_binary_target_in(
    root_dir: &Path,
    agent: &RegistryAgent,
    platform: Platform,
    target: &BinaryTarget,
) -> Result<CachedBinary> {
    cache_binary_target_in_mode(root_dir, agent, platform, target, false).await
}

/// Revalidates the registry target and rebuilds only a missing or corrupted
/// cache. Digest-bound cache keys are immutable, so a valid entry is never
/// replaced merely because an update was requested.
pub(crate) async fn refresh_binary_target_in(
    root_dir: &Path,
    agent: &RegistryAgent,
    platform: Platform,
    target: &BinaryTarget,
) -> Result<CachedBinary> {
    let result = cache_binary_target_in_mode(root_dir, agent, platform, target, true).await;
    record_install_log_in(root_dir, agent, platform, &result).await;
    result
}

async fn cache_binary_target_in_mode(
    root_dir: &Path,
    agent: &RegistryAgent,
    platform: Platform,
    target: &BinaryTarget,
    force_refresh: bool,
) -> Result<CachedBinary> {
    let digest = target
        .sha256
        .as_deref()
        .ok_or_else(|| anyhow!("binary target is missing required sha256 checksum"))?;
    // Reject malformed registry metadata before deriving a cache path or
    // starting a network request. This keeps integrity failures closed.
    parse_sha256(digest)?;
    let paths =
        binary_cache_paths_with_digest(root_dir, &agent.id, &agent.version, platform, digest);
    let expected = BinaryCacheMetadata::new(
        &agent.id,
        &agent.version,
        platform,
        &target.archive,
        &target.cmd,
        Some(digest),
    );

    fs::create_dir_all(&paths.parent_dir)
        .await
        .with_context(|| format!("failed to create {}", paths.parent_dir.display()))?;

    // Serialize publishers, removers, and the stale-work sweep before a work
    // directory is created. Holding the lock for the full prepare/publish
    // transaction prevents another process from mistaking active staging for
    // abandoned state.
    let lock = Arc::new(acquire_binary_cache_lock(&paths).await?);
    let use_lease = Arc::new(acquire_binary_cache_use_read_lock(&paths).await?);
    if let Some(prepared) =
        validate_cached_binary_with_lease(&paths, &expected, Some(Arc::clone(&use_lease))).await?
    {
        return Ok(CachedBinary {
            cache_use_lease: Some(use_lease),
            ..prepared
        });
    }
    drop(use_lease);

    // Stage the new cache in a `tempfile` guard so the payload is removed no
    // matter how this future ends: an explicit error, a panic, or a
    // cancellation while awaiting the download, extraction, or metadata
    // write. The staged payload is renamed into the stable cache directory
    // only after it has been fully prepared.
    let staging = tempfile::Builder::new()
        .prefix(&format!(
            ".{}-staging-",
            paths
                .cache_dir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("cache")
        ))
        .tempdir_in(&paths.parent_dir)
        .with_context(|| {
            format!(
                "failed to create staging directory in {}",
                paths.parent_dir.display()
            )
        })?;

    let staging = prepare_staging_directory(staging, target, &expected, Arc::clone(&lock)).await?;

    // Keep readers out while an invalid or old final directory is replaced.
    let publish_use_lock = Arc::new(acquire_binary_cache_use_write_lock(&paths).await?);
    let cached = promote_prepared_cache(
        staging,
        &paths,
        &expected,
        force_refresh,
        Some(Arc::clone(&publish_use_lock)),
    )
    .await?;
    drop(publish_use_lock);
    // The publication lock remains held while this shared lease is acquired,
    // closing the gap in which uninstall could otherwise remove the payload.
    let use_lease = acquire_binary_cache_use_read_lock(&paths).await?;
    drop(lock);
    Ok(CachedBinary {
        cache_use_lease: Some(Arc::new(use_lease)),
        ..cached
    })
}

/// Publishes a prepared cache in one blocking transaction.
///
/// The blocking task owns the staging directory and both cache locks. Dropping
/// the awaiting install future therefore only detaches a transaction that will
/// finish or roll back before releasing its locks; cancellation cannot strand
/// a backup or race a detached filesystem operation.
async fn promote_prepared_cache(
    staging: PreparedStaging,
    paths: &BinaryCachePaths,
    expected: &BinaryCacheMetadata,
    replace_existing: bool,
    cache_use_lease: Option<Arc<BinaryCacheLock>>,
) -> Result<CachedBinary> {
    let paths = paths.clone();
    let expected = expected.clone();
    tokio::task::spawn_blocking(move || {
        let _cache_use_lease = cache_use_lease;
        PromotionTransaction::new(staging, paths).run(&expected, replace_existing)
    })
    .await
    .map_err(|error| anyhow!("cache promotion task failed: {error}"))?
}

#[cfg(test)]
async fn promote_staged_cache(
    staging_dir: &Path,
    paths: &BinaryCachePaths,
    expected: &BinaryCacheMetadata,
    replace_existing: bool,
    cache_use_lease: Option<Arc<BinaryCacheLock>>,
) -> Result<CachedBinary> {
    promote_prepared_cache(
        PreparedStaging::without_lock(staging_dir.to_path_buf()),
        paths,
        expected,
        replace_existing,
        cache_use_lease,
    )
    .await
}

/// Removes staging and backup directories left behind by interrupted installs.
///
/// [`cache_binary_target`] stages new caches in a `tempfile` directory that is
/// removed automatically when the install future ends, but a process that is
/// killed (or a machine that crashes) mid-install never runs those drops.
/// This sweep is invoked at process startup as a recovery measure and removes
/// every dot-prefixed work directory under the cache's agents tree, including
/// the `-backup-` directories used to preserve a cache during a forced
/// refresh.
pub async fn clean_stale_staging_entries() -> Result<usize> {
    let root_dir = cache_root_dir()?;
    Ok(clean_stale_staging_entries_in(&root_dir).await)
}

/// Removes dot-prefixed work directories under `root_dir/agents/**` and
/// reports how many were removed. Errors are swallowed: the sweep is a
/// best-effort recovery measure and must never fail its caller.
async fn clean_stale_staging_entries_in(root_dir: &Path) -> usize {
    let agents_dir = root_dir.join(AGENTS_DIR);
    let mut removed = 0;
    let mut candidates: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    let Ok(mut agent_entries) = fs::read_dir(&agents_dir).await else {
        return removed;
    };
    while let Ok(Some(agent_entry)) = agent_entries.next_entry().await {
        let Ok(mut platform_entries) = fs::read_dir(agent_entry.path()).await else {
            continue;
        };
        while let Ok(Some(platform_entry)) = platform_entries.next_entry().await {
            let Ok(mut version_entries) = fs::read_dir(platform_entry.path()).await else {
                continue;
            };
            while let Ok(Some(version_entry)) = version_entries.next_entry().await {
                let file_name = version_entry.file_name();
                let file_name = file_name.to_string_lossy();
                let Some(cache_key) = work_dir_cache_key(&file_name) else {
                    continue;
                };
                let cache_dir = platform_entry.path().join(cache_key);
                candidates
                    .entry(cache_dir)
                    .or_default()
                    .push(version_entry.path());
            }
        }
    }

    // Group staging and backup directories by cache key so two abandoned
    // siblings do not race each other while the asynchronous lock-release
    // worker is still unwinding. One try-lock protects the complete sweep for
    // that key and an active publisher causes the whole group to be skipped.
    for (cache_dir, entries) in candidates {
        let paths = BinaryCachePaths {
            root_dir: root_dir.to_path_buf(),
            parent_dir: cache_dir
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| root_dir.to_path_buf()),
            extracted_dir: cache_dir.join(EXTRACTED_DIR_NAME),
            metadata_path: cache_dir.join(METADATA_FILE_NAME),
            cache_dir,
        };
        let Ok(Some(_lock)) = try_acquire_binary_cache_lock(&paths).await else {
            continue;
        };

        // A process can die after moving the previous final cache to its
        // backup but before publishing the replacement. Restore that backup
        // before deleting work directories; otherwise startup recovery would
        // turn an interrupted refresh into permanent cache loss.
        let Ok(final_exists) = fs::try_exists(&paths.cache_dir).await else {
            // Do not delete a backup when the final-entry probe itself failed;
            // preserving recoverable bytes is safer than guessing that the
            // destination exists.
            continue;
        };
        let mut restored_backup = None;
        if !final_exists {
            let mut backups = entries
                .iter()
                .filter(|entry| {
                    entry
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().contains("-backup-"))
                })
                .cloned()
                .collect::<Vec<_>>();
            backups.sort();
            for backup in backups.into_iter().rev() {
                if fs::rename(&backup, &paths.cache_dir).await.is_ok() {
                    restored_backup = Some(backup);
                    break;
                }
            }
        }
        for entry in entries {
            if restored_backup
                .as_ref()
                .is_some_and(|backup| backup == &entry)
            {
                continue;
            }
            if fs::remove_dir_all(entry).await.is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

fn work_dir_cache_key(name: &str) -> Option<&str> {
    let name = name.strip_prefix('.')?;
    ["-staging-", "-backup-"]
        .into_iter()
        .filter_map(|marker| name.rfind(marker).map(|index| (index, &name[..index])))
        .max_by_key(|(index, _)| *index)
        .map(|(_, key)| key)
        .filter(|key| !key.is_empty())
}

/// Appends one line per binary install attempt to `agent-install.log`.
///
/// Successes are logged too (cache hits included): a `ready` line is the only
/// way to confirm from outside the shell-less container which agent versions
/// are present in `/cache`; failures carry the full error chain. The blocking
/// file I/O runs on a blocking thread so a slow cache volume never stalls a
/// Tokio worker.
async fn record_install_log(
    agent: &RegistryAgent,
    platform: Platform,
    result: &Result<CachedBinary>,
) {
    let Ok(root_dir) = cache_root_dir() else {
        return;
    };
    record_install_log_in(&root_dir, agent, platform, result).await;
}

async fn record_install_log_in(
    root_dir: &Path,
    agent: &RegistryAgent,
    platform: Platform,
    result: &Result<CachedBinary>,
) {
    let platform = platform_cache_key(platform);
    let outcome = match result {
        Ok(cached) => format!(
            "ready agent={} version={} platform={} executable={}",
            agent.id,
            agent.version,
            platform,
            cached.executable_path.display()
        ),
        Err(error) => format!(
            "FAILED agent={} version={} platform={} error={error:#}",
            agent.id, agent.version, platform
        ),
    };
    let line = format!("[{}] {outcome}\n", utc_timestamp());
    let log_path = root_dir.join(INSTALL_LOG_FILE_NAME);
    let _ = tokio::task::spawn_blocking(move || append_install_log(&log_path, &line)).await;
}

fn append_install_log(path: &Path, line: &str) {
    if let Err(error) = append_install_log_inner(path, line) {
        eprintln!(
            "failed to append to agent install log {}: {error}",
            path.display()
        );
    }
}

fn append_install_log_inner(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Writers serialize on a dedicated lock file that is never renamed, so the
    // exclusive lock stays valid even though the atomic truncation below
    // replaces the log file with a new inode.
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(install_log_lock_path(path))?;
    let mut lock = fd_lock::RwLock::new(lock_file);
    let _guard = lock.write()?;

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    if file.metadata()?.len() + line.len() as u64 > INSTALL_LOG_MAX_BYTES {
        drop(file);
        truncate_install_log(path)?;
        file = std::fs::OpenOptions::new().append(true).open(path)?;
    }
    file.write_all(line.as_bytes())
}

/// Dedicated cross-process lock file guarding [`append_install_log_inner`].
///
/// A sibling of the log file (never the log file itself) so that the lock
/// stays associated with one stable inode across atomic log rotation.
fn install_log_lock_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("install.log"))
        .to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

/// Keeps only the most recent tail so the append-only log stays bounded in a
/// long-lived cache volume; the leading partial line is dropped so retained
/// lines stay complete. The rewrite lands in a temporary sibling file that is
/// renamed into place while the caller holds the cross-process lock, so a
/// concurrent reader observes either the old or the new log, never a torn one.
fn truncate_install_log(path: &Path) -> std::io::Result<()> {
    let bytes = std::fs::read(path)?;
    let mut kept = bytes
        .iter()
        .copied()
        .skip(bytes.len().saturating_sub(INSTALL_LOG_TAIL_BYTES as usize))
        .collect::<Vec<_>>();
    // Drop the leading partial line so every retained line is complete.
    if let Some(newline) = kept.iter().position(|&byte| byte == b'\n') {
        kept.drain(..=newline);
    }
    let mut rewritten = format!(
        "[install log truncated; keeping the last {} bytes]\n",
        INSTALL_LOG_TAIL_BYTES
    )
    .into_bytes();
    rewritten.append(&mut kept);

    let temp_path = path.with_extension("log.tmp");
    std::fs::write(&temp_path, rewritten)?;
    std::fs::rename(&temp_path, path)
}

/// UTC timestamp in `YYYY-MM-DDTHH:MM:SSZ` form.
fn utc_timestamp() -> String {
    let now = OffsetDateTime::now_utc();
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z",
        year = now.year(),
        month = now.month() as u8,
        day = now.day(),
        hour = now.hour(),
        minute = now.minute(),
        second = now.second(),
    )
}

pub(crate) async fn download_archive(target: &BinaryTarget, temp_dir: &Path) -> Result<PathBuf> {
    download_archive_with_limits(target, temp_dir, ArchiveLimits::default()).await
}

pub(crate) async fn download_archive_with_limits(
    target: &BinaryTarget,
    temp_dir: &Path,
    limits: ArchiveLimits,
) -> Result<PathBuf> {
    let expected_digest = target
        .sha256
        .as_deref()
        .ok_or_else(|| anyhow!("binary target is missing required sha256 checksum"))?;
    let expected_digest = parse_sha256(expected_digest)?;
    let url = reqwest::Url::parse(&target.archive)
        .with_context(|| format!("invalid archive URL: {}", target.archive))?;
    let archive_name = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|segment| !segment.is_empty())
        .unwrap_or("download.bin");
    validate_archive_component(archive_name)
        .with_context(|| format!("unsafe archive filename in URL: {archive_name}"))?;
    let destination = temp_dir.join(archive_name);

    let client = reqwest::Client::builder()
        .connect_timeout(limits.connect_timeout)
        .read_timeout(limits.read_timeout)
        .timeout(limits.total_timeout)
        .build()
        .context("failed to build archive HTTP client")?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to download archive from {}", target.archive))?;
    let response = response
        .error_for_status()
        .with_context(|| format!("failed to download archive from {}", target.archive))?;
    if let Some(length) = response.content_length()
        && length > limits.max_download_bytes
    {
        bail!(
            "archive exceeds download limit of {} bytes",
            limits.max_download_bytes
        );
    }
    let result: Result<()> = async {
        let mut stream = response.bytes_stream();
        // Keep file ownership in this future. Tokio filesystem writes use
        // detached blocking operations that can outlive cancellation and race
        // TempDir cleanup; a bounded response chunk is written synchronously
        // so the handle is always closed before staging is removed.
        let mut file = std::fs::File::create(&destination)
            .with_context(|| format!("failed to create {}", destination.display()))?;
        let mut digest = Sha256::new();
        let mut total = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| {
                format!("failed to read archive response from {}", target.archive)
            })?;
            total = total.saturating_add(chunk.len() as u64);
            if total > limits.max_download_bytes {
                bail!(
                    "archive exceeds download limit of {} bytes",
                    limits.max_download_bytes
                );
            }
            digest.update(&chunk);
            file.write_all(&chunk).with_context(|| {
                format!(
                    "failed to write downloaded archive to {}",
                    destination.display()
                )
            })?;
        }
        file.flush()?;
        let actual_digest = digest.finalize();
        if actual_digest.as_slice() != expected_digest {
            bail!(
                "sha256 checksum mismatch: expected {}, got {}",
                hex_encode(&expected_digest),
                hex_encode(actual_digest.as_slice())
            );
        }
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = std::fs::remove_file(&destination);
    }
    result.map(|()| destination)
}

/// Verifies downloaded bytes against the registry-declared SHA-256 digest.
///
/// A missing or malformed digest is rejected before any downloaded content is
/// accepted.
#[cfg(test)]
fn verify_sha256(bytes: &[u8], expected: Option<&str>) -> Result<()> {
    let expected =
        expected.ok_or_else(|| anyhow!("binary target is missing required sha256 checksum"))?;
    let expected = parse_sha256(expected)?;
    let actual = Sha256::digest(bytes);
    if actual.as_slice() != expected {
        bail!(
            "sha256 checksum mismatch: expected {}, got {}",
            hex_encode(&expected),
            hex_encode(actual.as_slice())
        );
    }
    Ok(())
}

/// Parses a registry-declared SHA-256 hex string into raw bytes.
fn parse_sha256(value: &str) -> Result<[u8; 32]> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid sha256 checksum \"{value}\": expected 64 hexadecimal characters");
    }
    let mut digest = [0u8; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .expect("hex digits were validated above");
    }
    Ok(digest)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) async fn extract_archive(archive_path: PathBuf, destination: PathBuf) -> Result<()> {
    extract_archive_with_limits(archive_path, destination, ArchiveLimits::default()).await
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) async fn extract_archive_with_limits(
    archive_path: PathBuf,
    destination: PathBuf,
    limits: ArchiveLimits,
) -> Result<()> {
    let cancel = Arc::new(AtomicBool::new(false));
    let task_cancel = Arc::clone(&cancel);
    let handle = tokio::task::spawn_blocking(move || {
        extract_archive_blocking(&archive_path, &destination, &task_cancel, limits)
    });
    // Dropping this future (an aborted or cancelled install) sets the flag, so
    // the detached blocking extraction stops before the staging guard removes
    // the directory instead of racing it.
    CancellableExtraction {
        handle: Some(handle),
        cancel,
        cleanup_path: None,
        cache_lock: None,
    }
    .await
}

/// Extraction variant used by installation staging. Once extraction starts,
/// ownership of cleanup moves into the blocking task so cancellation cannot
/// delete a directory while an entry is still being written.
async fn extract_archive_with_cleanup(
    archive_path: PathBuf,
    destination: PathBuf,
    limits: ArchiveLimits,
    cleanup_path: PathBuf,
    cache_lock: Arc<BinaryCacheLock>,
) -> Result<()> {
    let cancel = Arc::new(AtomicBool::new(false));
    let task_cancel = Arc::clone(&cancel);
    let cleanup_on_error = cleanup_path.clone();
    let handle = tokio::task::spawn_blocking(move || {
        let result = extract_archive_blocking(&archive_path, &destination, &task_cancel, limits);
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&cleanup_on_error);
        }
        result
    });
    let mut extraction = CancellableExtraction {
        handle: Some(handle),
        cancel,
        cleanup_path: Some(cleanup_path),
        cache_lock: Some(cache_lock),
    };
    let result = (&mut extraction).await;
    if result.is_ok() {
        extraction.cleanup_path = None;
    }
    result
}

/// [`spawn_blocking`] join that signals a cancellation flag when dropped.
///
/// [`tokio::task::spawn_blocking`] tasks cannot be cancelled directly: dropping
/// the [`JoinHandle`] merely detaches them, so an aborted install would leave a
/// writer running against a directory the staging guard is deleting. Setting
/// the flag from `Drop` lets the blocking extraction cooperate with the
/// cancellation before the guard's removal runs.
struct CancellableExtraction {
    handle: Option<tokio::task::JoinHandle<Result<()>>>,
    cancel: Arc<AtomicBool>,
    cleanup_path: Option<PathBuf>,
    cache_lock: Option<Arc<BinaryCacheLock>>,
}

impl Future for CancellableExtraction {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let handle = self
            .handle
            .as_mut()
            .expect("extraction future polled after completion");
        match Pin::new(handle).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(result)) => {
                self.handle.take();
                Poll::Ready(result)
            }
            Poll::Ready(Err(error)) => {
                self.handle.take();
                Poll::Ready(Err(anyhow!("extraction task failed: {error}")))
            }
        }
    }
}

impl Drop for CancellableExtraction {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        let Some(path) = self.cleanup_path.take() else {
            return;
        };
        let Some(handle) = self.handle.take() else {
            let _ = std::fs::remove_dir_all(path);
            return;
        };
        let cache_lock = self.cache_lock.take();
        // A blocking extractor cannot be aborted by dropping its JoinHandle.
        // Keep the handle alive until it observes the flag, then remove the
        // staging directory after the last write has completed.
        tokio::spawn(async move {
            let _ = handle.await;
            let _ = tokio::fs::remove_dir_all(path).await;
            drop(cache_lock);
        });
    }
}

fn extract_archive_blocking(
    archive_path: &Path,
    destination: &Path,
    cancel: &AtomicBool,
    limits: ArchiveLimits,
) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        bail!("extraction cancelled");
    }
    let file_name = archive_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if file_name.ends_with(".zip") {
        return extract_zip_with_limits(archive_path, destination, cancel, limits);
    }

    if file_name.ends_with(".tar.gz") || file_name.ends_with(".tgz") {
        let file = File::open(archive_path)
            .with_context(|| format!("failed to open archive {}", archive_path.display()))?;
        let decoder = CancellableReader::new(GzDecoder::new(file), cancel);
        return extract_tar(decoder, destination, cancel, limits);
    }

    if file_name.ends_with(".tar.bz2") || file_name.ends_with(".tbz2") {
        let file = File::open(archive_path)
            .with_context(|| format!("failed to open archive {}", archive_path.display()))?;
        let decoder = CancellableReader::new(BzDecoder::new(file), cancel);
        return extract_tar(decoder, destination, cancel, limits);
    }

    let file_name = archive_path
        .file_name()
        .ok_or_else(|| anyhow!("unsupported archive format for {}", archive_path.display()))?;
    if cancel.load(Ordering::Relaxed) {
        bail!("extraction cancelled");
    }
    let size = std::fs::metadata(archive_path)
        .with_context(|| format!("failed to inspect archive {}", archive_path.display()))?
        .len();
    if size > limits.max_expanded_bytes {
        bail!("expanded archive size limit exceeded");
    }
    validate_archive_component(
        file_name
            .to_str()
            .ok_or_else(|| anyhow!("archive filename is not valid UTF-8"))?,
    )?;
    let fallback_path = destination.join(file_name);
    let mut source = File::open(archive_path)
        .with_context(|| format!("failed to open archive {}", archive_path.display()))?;
    let mut output = File::create(&fallback_path)
        .with_context(|| format!("failed to create {}", fallback_path.display()))?;
    copy_with_limit(&mut source, &mut output, cancel, limits.max_expanded_bytes).with_context(
        || {
            format!(
                "failed to copy archive {} to {}",
                archive_path.display(),
                fallback_path.display()
            )
        },
    )?;
    Ok(())
}

struct CancellableReader<'a, R> {
    inner: R,
    cancel: &'a AtomicBool,
}

impl<'a, R> CancellableReader<'a, R> {
    fn new(inner: R, cancel: &'a AtomicBool) -> Self {
        Self { inner, cancel }
    }
}

impl<R: Read> Read for CancellableReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        check_extraction_cancelled(self.cancel)?;
        self.inner.read(buffer)
    }
}

fn check_extraction_cancelled(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "extraction cancelled",
        ))
    } else {
        Ok(())
    }
}

fn copy_with_limit<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    cancel: &AtomicBool,
    max_bytes: u64,
) -> io::Result<u64> {
    let mut buffer = [0u8; 32 * 1024];
    let mut copied = 0u64;
    loop {
        check_extraction_cancelled(cancel)?;
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(copied);
        }
        copied = copied.checked_add(read as u64).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "expanded archive size overflow")
        })?;
        if copied > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expanded archive size limit exceeded",
            ));
        }
        writer.write_all(&buffer[..read])?;
    }
}

/// Rejects archive paths that could escape the extraction root. Archive
/// formats may use either slash spelling regardless of the host platform, so
/// backslashes are rejected explicitly as well.
fn validate_archive_path(path: &Path) -> Result<()> {
    let text = path
        .to_str()
        .ok_or_else(|| anyhow!("archive path is not valid UTF-8"))?;
    if text.contains('\\') || text.contains('\0') {
        bail!("unsafe archive path: {text:?}");
    }
    let mut has_normal = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => has_normal = true,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("unsafe archive path: {text:?}");
            }
        }
    }
    if !has_normal {
        bail!("empty archive path");
    }
    Ok(())
}

fn validate_archive_component(component: &str) -> Result<()> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.contains('/')
        || component.contains('\\')
        || component.contains('\0')
    {
        bail!("unsafe archive filename: {component:?}");
    }
    Ok(())
}

/// Extracts a tar archive entry by entry, checking the cancellation flag
/// between entries so an aborted install stops writing promptly.
fn extract_tar<R: io::Read>(
    reader: R,
    destination: &Path,
    cancel: &AtomicBool,
    limits: ArchiveLimits,
) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    let entries = archive.entries().with_context(|| {
        format!(
            "failed to read archive entries for {}",
            destination.display()
        )
    })?;
    let mut entries_seen = 0u64;
    let mut expanded = 0u64;
    let mut files = 0u64;
    for entry in entries {
        if cancel.load(Ordering::Relaxed) {
            bail!("extraction cancelled");
        }
        let mut entry = entry.with_context(|| {
            format!(
                "failed to read an archive entry for {}",
                destination.display()
            )
        })?;
        let entry_path = entry.path()?;
        validate_archive_path(&entry_path)?;
        if let Some(link) = entry.link_name()? {
            validate_archive_path(&link)?;
        }
        entries_seen += 1;
        if entries_seen > limits.max_entries {
            bail!("archive entry limit exceeded");
        }
        let size = entry.size();
        expanded = expanded
            .checked_add(size)
            .ok_or_else(|| anyhow!("expanded archive size overflow"))?;
        if expanded > limits.max_expanded_bytes {
            bail!("expanded archive size limit exceeded");
        }
        if !entry.header().entry_type().is_dir() {
            files += 1;
            if files > limits.max_files {
                bail!("archive file limit exceeded");
            }
        }
        entry
            .unpack_in(destination)
            .with_context(|| format!("failed to unpack archive into {}", destination.display()))?;
    }
    Ok(())
}

#[cfg(test)]
fn extract_zip(archive_path: &Path, destination: &Path, cancel: &AtomicBool) -> Result<()> {
    extract_zip_with_limits(archive_path, destination, cancel, ArchiveLimits::default())
}

fn extract_zip_with_limits(
    archive_path: &Path,
    destination: &Path,
    cancel: &AtomicBool,
    limits: ArchiveLimits,
) -> Result<()> {
    let file = File::open(archive_path)
        .with_context(|| format!("failed to open archive {}", archive_path.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("failed to read ZIP archive {}", archive_path.display()))?;

    // Modes are applied in a second pass (children first) so a read-only
    // directory entry cannot prevent its own contents from being extracted.
    #[cfg(unix)]
    let mut unix_modes: Vec<(PathBuf, u32)> = Vec::new();

    let mut expanded = 0u64;
    let mut files = 0u64;
    if archive.len() as u64 > limits.max_entries {
        bail!("archive entry limit exceeded");
    }
    for index in 0..archive.len() {
        if cancel.load(Ordering::Relaxed) {
            bail!("extraction cancelled");
        }
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("failed to read ZIP entry {index}"))?;
        let enclosed = entry.enclosed_name().ok_or_else(|| {
            anyhow!(
                "unsafe path in ZIP archive {}: entry {index}",
                archive_path.display()
            )
        })?;
        validate_archive_path(&enclosed)?;
        let outpath = destination.join(enclosed);
        let expanded_before_entry = expanded;
        let declared_size = entry.size();
        expanded = expanded
            .checked_add(declared_size)
            .ok_or_else(|| anyhow!("expanded archive size overflow"))?;
        if expanded > limits.max_expanded_bytes {
            bail!("expanded archive size limit exceeded");
        }

        if entry.is_dir() {
            std::fs::create_dir_all(&outpath)
                .with_context(|| format!("failed to create directory {}", outpath.display()))?;
            #[cfg(unix)]
            if let Some(mode) = entry.unix_mode() {
                unix_modes.push((outpath, mode));
            }
            continue;
        }

        files += 1;
        if files > limits.max_files {
            bail!("archive file limit exceeded");
        }

        if entry.is_symlink() {
            extract_zip_symlink(
                &mut entry,
                &outpath,
                cancel,
                limits.max_expanded_bytes - expanded_before_entry,
            )?;
            continue;
        }

        if let Some(parent) = outpath.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let mut outfile = File::create(&outpath)
            .with_context(|| format!("failed to create {}", outpath.display()))?;
        let copied = copy_with_limit(
            &mut entry,
            &mut outfile,
            cancel,
            limits.max_expanded_bytes - expanded_before_entry,
        )
        .with_context(|| format!("failed to write {}", outpath.display()))?;
        if copied != declared_size {
            bail!(
                "ZIP entry size mismatch for {}: declared {declared_size}, extracted {copied}",
                outpath.display()
            );
        }

        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            unix_modes.push((outpath, mode));
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        for (path, mode) in unix_modes.into_iter().rev() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                .with_context(|| format!("failed to set permissions on {}", path.display()))?;
        }
    }

    Ok(())
}

/// Creates a symlink entry from a ZIP archive. Symbolic links require the
/// archive to record the target as the entry body; on platforms without
/// symlink support the entry is rejected.
fn extract_zip_symlink<R: Read>(
    entry: &mut zip::read::ZipFile<'_, R>,
    outpath: &Path,
    cancel: &AtomicBool,
    max_bytes: u64,
) -> Result<()> {
    let declared_size = entry.size();
    let mut target = Vec::new();
    let copied = copy_with_limit(entry, &mut target, cancel, max_bytes)
        .with_context(|| format!("failed to read symlink target for {}", outpath.display()))?;
    if copied != declared_size {
        bail!(
            "ZIP symlink size mismatch for {}: declared {declared_size}, extracted {copied}",
            outpath.display()
        );
    }
    let target = String::from_utf8(target).with_context(|| {
        format!(
            "symlink target for {} is not valid UTF-8",
            outpath.display()
        )
    })?;
    validate_archive_path(Path::new(&target))
        .with_context(|| format!("unsafe symlink target for {}", outpath.display()))?;

    if let Some(parent) = outpath.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, outpath)
            .with_context(|| format!("failed to create symlink {}", outpath.display()))?;
    }
    #[cfg(windows)]
    {
        let target_is_dir = std::fs::metadata(&target)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false);
        if target_is_dir {
            std::os::windows::fs::symlink_dir(&target, outpath)
                .with_context(|| format!("failed to create symlink {}", outpath.display()))?;
        } else {
            std::os::windows::fs::symlink_file(&target, outpath)
                .with_context(|| format!("failed to create symlink {}", outpath.display()))?;
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        bail!("symlinks are not supported on this platform");
    }
    Ok(())
}

pub(crate) fn resolve_cmd_path(extracted_dir: &Path, cmd: &str) -> Result<PathBuf> {
    let sanitized = cmd.trim();
    let candidate = PathBuf::from(sanitized);
    if candidate.is_absolute() {
        bail!("binary command path must be relative to the extracted payload: {cmd}");
    }

    let mut resolved = extracted_dir.to_path_buf();
    for component in candidate.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                bail!("binary command path must stay within the extracted payload: {cmd}");
            }
            Component::Prefix(_) | Component::RootDir => {
                bail!("binary command path must be relative to the extracted payload: {cmd}");
            }
            other => resolved.push(other.as_os_str()),
        }
    }

    Ok(resolved)
}

#[cfg(test)]
pub(crate) async fn make_executable(path: &Path) -> Result<(), io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path).await?.permissions();
        // Preserve the archive's permission policy and add only the owner
        // execute bit, so a private `0700` or group-limited `0750` executable
        // is not broadened to world-readable/executable.
        permissions.set_mode(permissions.mode() | 0o100);
        fs::set_permissions(path, permissions).await?;
    }

    #[cfg(not(unix))]
    {
        let _ = path;
    }

    Ok(())
}

async fn prepare_staging_directory(
    staging: tempfile::TempDir,
    target: &BinaryTarget,
    metadata: &BinaryCacheMetadata,
    cache_lock: Arc<BinaryCacheLock>,
) -> Result<PreparedStaging> {
    let staging_dir = staging.path().to_path_buf();
    let archive_path = download_archive(target, &staging_dir).await?;
    let extracted_dir = staging_dir.join(EXTRACTED_DIR_NAME);
    fs::create_dir_all(&extracted_dir)
        .await
        .with_context(|| format!("failed to create {}", extracted_dir.display()))?;
    // The blocking extractor now owns cleanup if cancellation occurs. Keep
    // the TempDir from racing it, then promotion takes ownership on success.
    let staging_path = staging.keep();
    let extraction = extract_archive_with_cleanup(
        archive_path,
        extracted_dir.clone(),
        ArchiveLimits::default(),
        staging_path.clone(),
        Arc::clone(&cache_lock),
    )
    .await;
    if let Err(error) = extraction {
        cleanup_dir(&staging_path).await;
        return Err(error);
    }
    let executable_path = match resolve_cmd_path(&extracted_dir, &target.cmd) {
        Ok(path) => path,
        Err(error) => {
            cleanup_dir(&staging_path).await;
            return Err(error);
        }
    };
    let metadata_path = staging_dir.join(METADATA_FILE_NAME);
    // Keep every post-extraction filesystem operation owned by one blocking
    // task. If this future is cancelled, its Drop implementation waits for
    // that task before removing staging, including on platforms that keep
    // files open while hashing or writing metadata.
    let mut validation = PostExtractionValidation::new(
        staging_path,
        executable_path,
        extracted_dir.clone(),
        metadata_path,
        metadata.clone(),
        cache_lock,
    );
    (&mut validation).await?;
    Ok(validation.disarm())
}

fn hash_file_sha256_blocking(path: &Path, cancel: &AtomicBool) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("failed to open executable {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 32 * 1024];
    loop {
        check_extraction_cancelled(cancel)?;
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    check_extraction_cancelled(cancel)?;
    Ok(hex_encode(hasher.finalize().as_slice()))
}

/// Hash the complete extracted tree in deterministic path order without
/// following symlinks. Entry names and kinds are included so layout changes
/// cannot preserve the same payload digest.
#[cfg(test)]
async fn hash_payload_sha256(path: &Path) -> Result<String> {
    let path = path.to_path_buf();
    let cancel = Arc::new(AtomicBool::new(false));
    let task_cancel = Arc::clone(&cancel);
    tokio::task::spawn_blocking(move || hash_payload_sha256_blocking(&path, &task_cancel))
        .await
        .map_err(|error| anyhow!("payload hash task failed: {error}"))?
}

fn hash_payload_sha256_blocking(path: &Path, cancel: &AtomicBool) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"acp-agent-payload-tree-v2\0");
    hash_payload_entry(path, Path::new(""), &mut hasher, cancel)?;
    Ok(hex_encode(hasher.finalize().as_slice()))
}

fn hash_length_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn path_hash_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        path.as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect()
    }
    #[cfg(not(any(unix, windows)))]
    {
        path.to_string_lossy().as_bytes().to_vec()
    }
}

fn hash_payload_entry(
    path: &Path,
    relative: &Path,
    hasher: &mut Sha256,
    cancel: &AtomicBool,
) -> Result<()> {
    check_extraction_cancelled(cancel)?;
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect payload entry {}", path.display()))?;
    let kind = metadata.file_type();
    hash_length_prefixed(hasher, &path_hash_bytes(relative));
    if kind.is_dir() {
        hasher.update(b"d");
        let mut entries = std::fs::read_dir(path)
            .with_context(|| format!("failed to read payload directory {}", path.display()))?
            .collect::<std::result::Result<Vec<_>, std::io::Error>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        hasher.update((entries.len() as u64).to_le_bytes());
        for entry in entries {
            let name = entry.file_name();
            hash_payload_entry(&entry.path(), &relative.join(name), hasher, cancel)?;
        }
    } else if kind.is_file() {
        hasher.update(b"f");
        let expected_len = metadata.len();
        hasher.update(expected_len.to_le_bytes());
        let mut file = std::fs::File::open(path)
            .with_context(|| format!("failed to open payload file {}", path.display()))?;
        let mut buffer = [0u8; 32 * 1024];
        let mut actual_len = 0u64;
        loop {
            check_extraction_cancelled(cancel)?;
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            actual_len = actual_len
                .checked_add(read as u64)
                .ok_or_else(|| anyhow!("payload file is too large to hash: {}", path.display()))?;
            hasher.update(&buffer[..read]);
        }
        if actual_len != expected_len {
            bail!("payload file changed while hashing: {}", path.display());
        }
    } else if kind.is_symlink() {
        hasher.update(b"l");
        let target = std::fs::read_link(path)
            .with_context(|| format!("failed to read payload link {}", path.display()))?;
        hash_length_prefixed(hasher, &path_hash_bytes(&target));
    } else {
        hasher.update(b"o");
        hasher.update(metadata.len().to_le_bytes());
    }
    Ok(())
}

fn make_executable_blocking(path: &Path) -> Result<(), io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(permissions.mode() | 0o100);
        std::fs::set_permissions(path, permissions)?;
    }

    #[cfg(not(unix))]
    {
        let _ = path;
    }

    Ok(())
}

/// Owns a fully prepared staging directory between extraction and publication.
///
/// `TempDir::keep` is required while the cooperative blocking tasks are
/// running, but returning its path alone would reopen a cancellation leak
/// while the publisher waits for the payload-use writer lease. This guard keeps
/// both the path and publication lock alive until cleanup has finished.
struct PreparedStaging {
    path: Option<PathBuf>,
    cache_lock: Option<Arc<BinaryCacheLock>>,
}

impl PreparedStaging {
    fn new(path: PathBuf, cache_lock: Arc<BinaryCacheLock>) -> Self {
        Self {
            path: Some(path),
            cache_lock: Some(cache_lock),
        }
    }

    #[cfg(test)]
    fn without_lock(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            cache_lock: None,
        }
    }

    fn path(&self) -> &Path {
        self.path
            .as_deref()
            .expect("prepared staging path must remain owned")
    }

    fn disarm(&mut self) {
        self.path.take();
    }

    /// Removes the directory synchronously from a blocking worker. Keeping
    /// this operation synchronous is important: a dropped `tokio::fs` future
    /// can detach its own blocking worker and race a later publisher.
    fn cleanup_blocking(&mut self) -> std::io::Result<()> {
        let Some(path) = self.path.take() else {
            return Ok(());
        };
        match std::fs::remove_dir_all(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                self.path = Some(path);
                Err(error)
            }
        }
    }
}

impl Drop for PreparedStaging {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        let cache_lock = self.cache_lock.take();
        let cleanup = move || {
            let _ = std::fs::remove_dir_all(path);
            drop(cache_lock);
        };
        // The guard is normally dropped inside a Tokio task while waiting for
        // the use-write lease. Move cleanup to a blocking worker so a large
        // abandoned payload cannot stall the async runtime. The fallback is
        // for tests or teardown after the runtime has already gone away.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            drop(handle.spawn_blocking(cleanup));
        } else {
            cleanup();
        }
    }
}

/// Atomic publication state machine used by the detached blocking publisher.
///
/// The transaction owns the staging path and publication lock. If a rename or
/// post-publish validation fails, `Drop` removes the new directory and restores
/// the previous cache before releasing the lock.
struct PromotionTransaction {
    staging: PreparedStaging,
    paths: BinaryCachePaths,
    backup_dir: Option<PathBuf>,
    published: bool,
    committed: bool,
}

impl PromotionTransaction {
    fn new(staging: PreparedStaging, paths: BinaryCachePaths) -> Self {
        Self {
            staging,
            paths,
            backup_dir: None,
            published: false,
            committed: false,
        }
    }

    fn run(
        mut self,
        expected: &BinaryCacheMetadata,
        replace_existing: bool,
    ) -> Result<CachedBinary> {
        if self
            .paths
            .cache_dir
            .try_exists()
            .with_context(|| format!("failed to inspect {}", self.paths.cache_dir.display()))?
        {
            if !replace_existing
                && let Some(cached) = validate_cached_binary_blocking(&self.paths, expected)?
            {
                self.staging
                    .cleanup_blocking()
                    .with_context(|| "failed to discard superseded staging directory")?;
                self.committed = true;
                return Ok(cached);
            }

            let backup_dir = self
                .paths
                .parent_dir
                .join(unique_backup_dir_name(&self.paths.cache_dir));
            std::fs::rename(&self.paths.cache_dir, &backup_dir).with_context(|| {
                format!(
                    "failed to preserve existing cache directory {} before replacement",
                    self.paths.cache_dir.display()
                )
            })?;
            self.backup_dir = Some(backup_dir);
        }

        std::fs::rename(self.staging.path(), &self.paths.cache_dir).with_context(|| {
            format!(
                "failed to promote staged cache {} to {}",
                self.staging.path().display(),
                self.paths.cache_dir.display()
            )
        })?;
        self.published = true;

        let cached = validate_cached_binary_blocking(&self.paths, expected)?
            .ok_or_else(|| anyhow!("published cache failed post-publish validation"))?;
        self.committed = true;
        self.staging.disarm();
        if let Some(backup_dir) = self.backup_dir.take() {
            // A failed cleanup is recoverable by the startup sweep. The new
            // validated cache is already committed, so do not roll it back
            // merely because deleting an obsolete backup failed.
            let _ = std::fs::remove_dir_all(backup_dir);
        }
        Ok(cached)
    }
}

impl Drop for PromotionTransaction {
    fn drop(&mut self) {
        if self.committed {
            return;
        }

        if self.published {
            let _ = std::fs::remove_dir_all(&self.paths.cache_dir);
        }
        if let Some(backup_dir) = self.backup_dir.take()
            && !self.paths.cache_dir.exists()
        {
            let _ = std::fs::rename(backup_dir, &self.paths.cache_dir);
        }
        let _ = self.staging.cleanup_blocking();
    }
}

/// Owns all post-extraction validation work and the staging directory it
/// operates on. A dropped join handle is detached by Tokio, so cleanup waits
/// for the task explicitly before deleting the directory.
struct PostExtractionValidation {
    handle: Option<tokio::task::JoinHandle<Result<()>>>,
    cancel: Arc<AtomicBool>,
    cleanup_path: Option<PathBuf>,
    cache_lock: Option<Arc<BinaryCacheLock>>,
}

impl PostExtractionValidation {
    fn new(
        cleanup_path: PathBuf,
        executable_path: PathBuf,
        extracted_dir: PathBuf,
        metadata_path: PathBuf,
        mut metadata: BinaryCacheMetadata,
        cache_lock: Arc<BinaryCacheLock>,
    ) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let task_cancel = Arc::clone(&cancel);
        let handle = tokio::task::spawn_blocking(move || {
            let file_metadata = std::fs::metadata(&executable_path);
            if file_metadata
                .as_ref()
                .map(|metadata| !metadata.is_file())
                .unwrap_or(true)
            {
                bail!(
                    "downloaded payload does not contain executable {}",
                    executable_path.display()
                );
            }
            make_executable_blocking(&executable_path).with_context(|| {
                format!("failed to mark {} executable", executable_path.display())
            })?;
            let digest = hash_file_sha256_blocking(&executable_path, &task_cancel)?;
            metadata.executable_sha256 = Some(digest);
            metadata.payload_sha256 =
                Some(hash_payload_sha256_blocking(&extracted_dir, &task_cancel)?);
            check_extraction_cancelled(&task_cancel)?;
            let metadata_bytes =
                to_vec_pretty(&metadata).context("failed to encode cached binary metadata")?;
            check_extraction_cancelled(&task_cancel)?;
            std::fs::write(&metadata_path, metadata_bytes)
                .with_context(|| format!("failed to write {}", metadata_path.display()))?;
            Ok(())
        });
        Self {
            handle: Some(handle),
            cancel,
            cleanup_path: Some(cleanup_path),
            cache_lock: Some(cache_lock),
        }
    }

    fn disarm(mut self) -> PreparedStaging {
        let path = self
            .cleanup_path
            .take()
            .expect("post-extraction validation must own a path");
        let cache_lock = self
            .cache_lock
            .take()
            .expect("post-extraction validation must own the cache lock");
        PreparedStaging::new(path, cache_lock)
    }
}

impl Future for PostExtractionValidation {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let handle = self
            .handle
            .as_mut()
            .expect("post-extraction validation polled after completion");
        match Pin::new(handle).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(result)) => {
                self.handle.take();
                Poll::Ready(result)
            }
            Poll::Ready(Err(error)) => {
                self.handle.take();
                Poll::Ready(Err(anyhow!(
                    "post-extraction validation task failed: {error}"
                )))
            }
        }
    }
}

impl Drop for PostExtractionValidation {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        let Some(path) = self.cleanup_path.take() else {
            return;
        };
        let Some(handle) = self.handle.take() else {
            let _ = std::fs::remove_dir_all(path);
            return;
        };
        let cache_lock = self.cache_lock.take();
        tokio::spawn(async move {
            let _ = handle.await;
            let _ = tokio::fs::remove_dir_all(path).await;
            drop(cache_lock);
        });
    }
}

#[cfg(test)]
async fn validate_cached_binary(
    paths: &BinaryCachePaths,
    expected: &BinaryCacheMetadata,
) -> Result<Option<CachedBinary>> {
    validate_cached_binary_with_lease(paths, expected, None).await
}

async fn validate_cached_binary_with_lease(
    paths: &BinaryCachePaths,
    expected: &BinaryCacheMetadata,
    cache_use_lease: Option<Arc<BinaryCacheLock>>,
) -> Result<Option<CachedBinary>> {
    let paths = paths.clone();
    let expected = expected.clone();
    tokio::task::spawn_blocking(move || {
        // The lease is intentionally owned by this task: cancelling the async
        // caller detaches a Tokio blocking task, so deletion/replacement must
        // remain blocked until its last filesystem read has completed.
        let _cache_use_lease = cache_use_lease;
        validate_cached_binary_blocking(&paths, &expected)
    })
    .await
    .map_err(|error| anyhow!("cached binary validation task failed: {error}"))?
}

fn validate_cached_binary_blocking(
    paths: &BinaryCachePaths,
    expected: &BinaryCacheMetadata,
) -> Result<Option<CachedBinary>> {
    if !paths
        .metadata_path
        .try_exists()
        .with_context(|| format!("failed to inspect {}", paths.metadata_path.display()))?
    {
        return Ok(None);
    }

    let metadata_bytes = std::fs::read(&paths.metadata_path)
        .with_context(|| format!("failed to read {}", paths.metadata_path.display()))?;
    let metadata: BinaryCacheMetadata = match serde_json::from_slice(&metadata_bytes) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(None),
    };
    if metadata.agent_id != expected.agent_id
        || metadata.agent_version != expected.agent_version
        || metadata.platform != expected.platform
        || metadata.archive != expected.archive
        || metadata.cmd != expected.cmd
        || metadata.sha256 != expected.sha256
    {
        return Ok(None);
    }

    let executable_path = match resolve_cmd_path(&paths.extracted_dir, &metadata.cmd) {
        Ok(path) => path,
        Err(_) => return Ok(None),
    };
    let file_metadata = std::fs::metadata(&executable_path);
    if file_metadata
        .as_ref()
        .map(|metadata| !metadata.is_file())
        .unwrap_or(true)
    {
        return Ok(None);
    }

    // New digest-bound entries carry an executable digest. Verify it on every
    // cache hit so edits or truncation cannot be executed indefinitely.
    if let Some(expected_executable) = metadata.executable_sha256.as_deref() {
        let actual_executable =
            hash_file_sha256_blocking(&executable_path, &AtomicBool::new(false))?;
        if !expected_executable.eq_ignore_ascii_case(&actual_executable) {
            return Ok(None);
        }
    } else if expected.sha256.is_some() {
        // A digest-bound target must never accept metadata produced without an
        // executable digest. Digest-less entries are legacy-only and are
        // accepted above solely for migration compatibility.
        return Ok(None);
    }

    if let Some(expected_payload) = metadata.payload_sha256.as_deref() {
        let actual_payload =
            hash_payload_sha256_blocking(&paths.extracted_dir, &AtomicBool::new(false))?;
        if !expected_payload.eq_ignore_ascii_case(&actual_payload) {
            return Ok(None);
        }
    } else if expected.sha256.is_some() {
        // Rebuild digest-bound entries created before payload digests were
        // persisted instead of trusting an incomplete tree validation.
        return Ok(None);
    }

    Ok(Some(CachedBinary {
        executable_path,
        extracted_dir: paths.extracted_dir.clone(),
        cache_dir: paths.cache_dir.clone(),
        cache_use_lease: None,
    }))
}

async fn cleanup_dir(path: &Path) {
    let _ = fs::remove_dir_all(path).await;
}

fn unique_backup_dir_name(cache_dir: &Path) -> String {
    let version = cache_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("cache");
    unique_work_dir_name(version, "backup")
}

fn unique_work_dir_name(component: &str, kind: &str) -> String {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!(".{}-{kind}-{pid}-{nanos}", safe_path_component(component))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::installer::cache::BinaryCacheMetadata;
    use crate::installer::cache::binary_cache_paths;
    use crate::registry::{AgentDistribution, BinaryDistribution};
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn rejects_download_without_declared_sha256() {
        let error = verify_sha256(b"payload", None).unwrap_err();
        assert!(error.to_string().contains("missing required sha256"));
    }

    #[test]
    fn accepts_download_matching_declared_sha256() {
        let payload: &[u8] = b"payload";
        let digest = hex_encode(Sha256::digest(payload).as_slice());
        verify_sha256(payload, Some(&digest)).expect("matching digest should pass");
    }

    #[test]
    fn rejects_download_mismatching_declared_sha256() {
        let error = verify_sha256(b"payload", Some(&"0".repeat(64))).unwrap_err();
        assert!(error.to_string().contains("sha256 checksum mismatch"));
    }

    #[test]
    fn rejects_malformed_declared_sha256() {
        let error = verify_sha256(b"payload", Some("not-a-sha256")).unwrap_err();
        assert!(error.to_string().contains("invalid sha256 checksum"));
    }

    #[tokio::test]
    async fn rejects_missing_digest_before_url_parsing() {
        let temp_dir = tempdir().unwrap();
        let target = BinaryTarget {
            archive: "not a valid URL".to_string(),
            cmd: "tool".to_string(),
            sha256: None,
            args: None,
            env: None,
        };
        let error = download_archive(&target, temp_dir.path())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("missing required sha256"));
        assert!(temp_dir.path().read_dir().unwrap().next().is_none());
    }

    #[test]
    fn resolves_relative_cmd_paths() {
        let base = Path::new("/tmp/acp-agent");
        let resolved = resolve_cmd_path(base, "./dist-package/cursor-agent").unwrap();
        assert_eq!(resolved, base.join("dist-package").join("cursor-agent"));
    }

    #[test]
    fn rejects_absolute_cmd_paths() {
        let base = Path::new("/tmp/acp-agent");
        let error = resolve_cmd_path(base, "/bin/sh").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("binary command path must be relative")
        );
    }

    #[test]
    fn rejects_parent_dir_cmd_paths() {
        let base = Path::new("/tmp/acp-agent");
        let error = resolve_cmd_path(base, "../bin/sh").unwrap_err();
        assert!(error.to_string().contains("must stay within"));
    }

    #[tokio::test]
    async fn payload_hash_has_unambiguous_tree_boundaries() {
        let temp_dir = tempdir().unwrap();
        let two_files = temp_dir.path().join("two-files");
        let encoded_as_content = temp_dir.path().join("encoded-as-content");
        fs::create_dir_all(&two_files).await.unwrap();
        fs::create_dir_all(&encoded_as_content).await.unwrap();
        fs::write(two_files.join("a"), b"x").await.unwrap();
        fs::write(two_files.join("b"), b"y").await.unwrap();
        // This byte sequence collided with the old delimiter-only encoding:
        // it looks exactly like the end of `a` followed by a complete `b`.
        fs::write(
            encoded_as_content.join("a"),
            [b'x', 0xff, b'b', 0, b'f', 0, b'y'],
        )
        .await
        .unwrap();

        assert_ne!(
            hash_payload_sha256(&two_files).await.unwrap(),
            hash_payload_sha256(&encoded_as_content).await.unwrap()
        );
    }

    #[tokio::test]
    async fn validates_matching_cached_binary() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        let metadata = BinaryCacheMetadata::new(
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            "https://example.com/demo.tar.gz",
            "./bin/demo",
            Some("a".repeat(64).as_str()),
        );

        fs::create_dir_all(&paths.extracted_dir).await.unwrap();
        let executable_path = paths.extracted_dir.join("bin").join("demo");
        fs::create_dir_all(executable_path.parent().unwrap())
            .await
            .unwrap();
        fs::write(&executable_path, b"#!/bin/sh\n").await.unwrap();

        let mut metadata = metadata;
        metadata.executable_sha256 = Some(hex_encode(Sha256::digest(b"#!/bin/sh\n").as_slice()));
        metadata.payload_sha256 = Some(hash_payload_sha256(&paths.extracted_dir).await.unwrap());
        fs::write(&paths.metadata_path, serde_json::to_vec(&metadata).unwrap())
            .await
            .unwrap();

        let prepared = validate_cached_binary(&paths, &metadata).await.unwrap();
        assert_eq!(
            prepared.unwrap(),
            CachedBinary {
                executable_path,
                extracted_dir: paths.extracted_dir,
                cache_dir: paths.cache_dir,
                cache_use_lease: None,
            }
        );
    }

    #[tokio::test]
    async fn rejects_cached_binary_when_metadata_mismatches() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        let expected = BinaryCacheMetadata::new(
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            "https://example.com/demo.tar.gz",
            "./bin/demo",
            Some("a".repeat(64).as_str()),
        );
        let cached = BinaryCacheMetadata::new(
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            "https://example.com/demo.tar.gz",
            "./bin/demo",
            Some("b".repeat(64).as_str()),
        );

        fs::create_dir_all(&paths.extracted_dir).await.unwrap();
        fs::write(&paths.metadata_path, serde_json::to_vec(&cached).unwrap())
            .await
            .unwrap();

        assert!(
            validate_cached_binary(&paths, &expected)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn rejects_modified_digest_bound_executable() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths_with_digest(
            &cache_root,
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            &"a".repeat(64),
        );
        let mut metadata = BinaryCacheMetadata::new(
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            "https://example.com/demo.tar.gz",
            "./bin/demo",
            Some(&"a".repeat(64)),
        );
        fs::create_dir_all(paths.extracted_dir.join("bin"))
            .await
            .unwrap();
        let executable_path = paths.extracted_dir.join("bin/demo");
        fs::write(&executable_path, b"original").await.unwrap();
        metadata.executable_sha256 = Some(hex_encode(Sha256::digest(b"original").as_slice()));
        fs::write(&paths.metadata_path, serde_json::to_vec(&metadata).unwrap())
            .await
            .unwrap();

        fs::write(&executable_path, b"modified").await.unwrap();
        assert!(
            validate_cached_binary(&paths, &metadata)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn rejects_modified_digest_bound_payload_file() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths_with_digest(
            &cache_root,
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            &"a".repeat(64),
        );
        let mut metadata = BinaryCacheMetadata::new(
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            "https://example.com/demo.tar.gz",
            "./bin/demo",
            Some(&"a".repeat(64)),
        );
        fs::create_dir_all(paths.extracted_dir.join("bin"))
            .await
            .unwrap();
        fs::create_dir_all(paths.extracted_dir.join("lib"))
            .await
            .unwrap();
        let executable_path = paths.extracted_dir.join("bin/demo");
        fs::write(&executable_path, b"original").await.unwrap();
        fs::write(paths.extracted_dir.join("lib/helper"), b"helper")
            .await
            .unwrap();
        metadata.executable_sha256 = Some(hex_encode(Sha256::digest(b"original").as_slice()));
        metadata.payload_sha256 = Some(hash_payload_sha256(&paths.extracted_dir).await.unwrap());
        fs::write(&paths.metadata_path, serde_json::to_vec(&metadata).unwrap())
            .await
            .unwrap();

        fs::write(paths.extracted_dir.join("lib/helper"), b"modified")
            .await
            .unwrap();
        assert!(
            validate_cached_binary(&paths, &metadata)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn corrupted_metadata_is_treated_as_cache_miss_without_unleased_removal() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        let expected = BinaryCacheMetadata::new(
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            "https://example.com/demo.tar.gz",
            "./bin/demo",
            Some("a".repeat(64).as_str()),
        );

        fs::create_dir_all(&paths.cache_dir).await.unwrap();
        fs::write(&paths.metadata_path, b"{not-json").await.unwrap();

        assert!(
            validate_cached_binary(&paths, &expected)
                .await
                .unwrap()
                .is_none()
        );
        // Validation does not delete an entry on its own: callers must first
        // take the payload-use write lease so an active reader cannot race
        // the cleanup. Publication replaces this entry under that lease.
        assert!(fs::try_exists(&paths.cache_dir).await.unwrap());
    }

    #[tokio::test]
    async fn failed_promotion_restores_the_previous_cache() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        let previous = BinaryCacheMetadata::new(
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            "https://example.com/previous.tar.gz",
            "./bin/demo",
            None,
        );
        let replacement = BinaryCacheMetadata::new(
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            "https://example.com/replacement.tar.gz",
            "./bin/demo",
            None,
        );
        fs::create_dir_all(&paths.extracted_dir).await.unwrap();
        fs::write(&paths.metadata_path, serde_json::to_vec(&previous).unwrap())
            .await
            .unwrap();
        let executable = paths.extracted_dir.join("bin").join("demo");
        fs::create_dir_all(executable.parent().unwrap())
            .await
            .unwrap();
        fs::write(&executable, b"previous").await.unwrap();

        let missing_staging = paths.parent_dir.join(".missing-staging");
        assert!(
            promote_staged_cache(&missing_staging, &paths, &replacement, true, None)
                .await
                .is_err()
        );

        assert_eq!(fs::read(&executable).await.unwrap(), b"previous");
        let restored: BinaryCacheMetadata =
            serde_json::from_slice(&fs::read(&paths.metadata_path).await.unwrap()).unwrap();
        assert_eq!(restored, previous);
    }

    #[tokio::test]
    async fn forced_promotion_replaces_cache_with_unchanged_metadata() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        let metadata = BinaryCacheMetadata::new(
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            "https://example.com/demo.tar.gz",
            "./demo",
            None,
        );
        fs::create_dir_all(&paths.extracted_dir).await.unwrap();
        fs::write(&paths.metadata_path, serde_json::to_vec(&metadata).unwrap())
            .await
            .unwrap();
        fs::write(paths.extracted_dir.join("demo"), b"old")
            .await
            .unwrap();

        let staging_dir = paths.parent_dir.join(".staging");
        let staging_extracted = staging_dir.join(EXTRACTED_DIR_NAME);
        fs::create_dir_all(&staging_extracted).await.unwrap();
        fs::write(
            staging_dir.join(METADATA_FILE_NAME),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .await
        .unwrap();
        fs::write(staging_extracted.join("demo"), b"new")
            .await
            .unwrap();

        promote_staged_cache(&staging_dir, &paths, &metadata, true, None)
            .await
            .unwrap();

        assert_eq!(
            fs::read(paths.extracted_dir.join("demo")).await.unwrap(),
            b"new"
        );
    }

    #[test]
    fn install_log_appends_lines_in_order() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().join("agent-install.log");

        append_install_log_inner(&path, "first\n").unwrap();
        append_install_log_inner(&path, "second\n").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\nsecond\n");
    }

    #[test]
    fn install_log_lock_is_a_sibling_for_relative_paths() {
        assert_eq!(
            install_log_lock_path(Path::new("cache/agent-install.log")),
            PathBuf::from("cache/agent-install.log.lock")
        );
    }

    #[test]
    fn install_log_appends_concurrently_without_losing_lines() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().join("agent-install.log");
        let thread_count = 16;
        let lines_per_thread = 50;

        let mut threads = Vec::new();
        for t in 0..thread_count {
            let path = path.clone();
            threads.push(std::thread::spawn(move || {
                for i in 0..lines_per_thread {
                    append_install_log_inner(&path, &format!("thread-{t}-line-{i}\n")).unwrap();
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }

        let contents = std::fs::read_to_string(&path).unwrap();
        let mut lines = contents.lines().map(String::from).collect::<Vec<_>>();
        lines.sort_unstable();
        assert_eq!(lines.len(), thread_count * lines_per_thread);

        let mut expected = Vec::with_capacity(thread_count * lines_per_thread);
        for t in 0..thread_count {
            for i in 0..lines_per_thread {
                expected.push(format!("thread-{t}-line-{i}"));
            }
        }
        expected.sort_unstable();
        assert_eq!(lines, expected);
    }

    #[test]
    fn install_log_is_capped_and_keeps_a_complete_tail() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().join("agent-install.log");
        let oversized = "a".repeat(INSTALL_LOG_MAX_BYTES as usize + 64 * 1024);

        append_install_log_inner(&path, &oversized).unwrap();
        append_install_log_inner(&path, &oversized).unwrap();
        append_install_log_inner(&path, "final-marker\n").unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.len() as u64 <= INSTALL_LOG_MAX_BYTES,
            "log exceeded its cap: {}",
            contents.len()
        );
        assert!(contents.starts_with("[install log truncated;"));
        assert!(contents.ends_with("final-marker\n"));
        assert!(
            contents
                .lines()
                .all(|line| line.len() < INSTALL_LOG_MAX_BYTES as usize)
        );
    }

    #[test]
    fn timestamps_are_utc_iso_8601() {
        let timestamp = utc_timestamp();
        assert_eq!(timestamp.len(), 20);
        assert!(timestamp.ends_with('Z'));
        assert_eq!(&timestamp[4..5], "-");
        assert_eq!(&timestamp[7..8], "-");
        assert_eq!(&timestamp[10..11], "T");
        assert_eq!(&timestamp[13..14], ":");
        assert_eq!(&timestamp[16..17], ":");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn make_executable_preserves_mode_and_adds_only_owner_execute() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempdir().unwrap();
        for (source_mode, expected_mode) in [
            (0o600, 0o700),
            (0o700, 0o700),
            (0o750, 0o750),
            (0o755, 0o755),
        ] {
            let path = temp_dir.path().join(format!("tool-{source_mode:o}"));
            std::fs::write(&path, b"#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(source_mode)).unwrap();

            make_executable(&path).await.unwrap();

            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, expected_mode,
                "mode {source_mode:o} should become {expected_mode:o}"
            );
        }
    }

    // --- ZIP extraction ---

    /// Writes a ZIP archive whose entries are built through the provided
    /// closure, using the crate's own writer.
    fn build_zip_archive(archive_path: &Path, write: impl FnOnce(&mut zip::ZipWriter<File>)) {
        let file = File::create(archive_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        write(&mut writer);
        writer.finish().unwrap();
    }

    /// Hand-builds a single-entry stored (uncompressed) ZIP so tests can use
    /// entry names the writer would sanitize away, such as `../` traversal.
    fn build_raw_zip(archive_path: &Path, entry_name: &str, payload: &[u8]) {
        use std::io::Write;

        let crc = crc32_ieee(payload);
        let name = entry_name.as_bytes();

        let mut bytes = Vec::new();
        // Local file header.
        bytes.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        bytes.extend_from_slice(&20u16.to_le_bytes()); // version needed
        bytes.extend_from_slice(&0u16.to_le_bytes()); // flags
        bytes.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        bytes.extend_from_slice(&0u16.to_le_bytes()); // mod time
        bytes.extend_from_slice(&0x21u16.to_le_bytes()); // mod date (1980-01-01)
        bytes.extend_from_slice(&crc.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes()); // extra length
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(payload);

        let mut central = Vec::new();
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&0x031eu16.to_le_bytes()); // version made by (unix)
        central.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central.extend_from_slice(&0u16.to_le_bytes()); // flags
        central.extend_from_slice(&0u16.to_le_bytes()); // method
        central.extend_from_slice(&0u16.to_le_bytes()); // mod time
        central.extend_from_slice(&0x21u16.to_le_bytes()); // mod date
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        central.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra length
        central.extend_from_slice(&0u16.to_le_bytes()); // comment length
        central.extend_from_slice(&0u16.to_le_bytes()); // disk number start
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
        central.extend_from_slice(&(0o100644u32 << 16).to_le_bytes()); // external attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // local header offset
        central.extend_from_slice(name);

        let mut eocd = Vec::new();
        eocd.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        eocd.extend_from_slice(&0u16.to_le_bytes()); // this disk
        eocd.extend_from_slice(&0u16.to_le_bytes()); // cd disk
        eocd.extend_from_slice(&1u16.to_le_bytes()); // entries on this disk
        eocd.extend_from_slice(&1u16.to_le_bytes()); // total entries
        eocd.extend_from_slice(&(central.len() as u32).to_le_bytes());
        eocd.extend_from_slice(&(bytes.len() as u32).to_le_bytes()); // cd offset
        eocd.extend_from_slice(&0u16.to_le_bytes()); // comment length

        bytes.extend_from_slice(&central);
        bytes.extend_from_slice(&eocd);

        let mut file = File::create(archive_path).unwrap();
        file.write_all(&bytes).unwrap();
    }

    /// Standard CRC-32 (IEEE 802.3) for hand-built ZIP fixtures.
    fn crc32_ieee(data: &[u8]) -> u32 {
        let mut table = [0u32; 256];
        for (index, entry) in table.iter_mut().enumerate() {
            let mut value = index as u32;
            for _ in 0..8 {
                value = if value & 1 != 0 {
                    0xedb8_8320 ^ (value >> 1)
                } else {
                    value >> 1
                };
            }
            *entry = value;
        }
        let mut crc = 0xffff_ffffu32;
        for &byte in data {
            crc = table[((crc ^ byte as u32) & 0xff) as usize] ^ (crc >> 8);
        }
        !crc
    }

    #[test]
    fn zip_extracts_directory_entries_and_files() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let temp_dir = tempdir().unwrap();
        let archive_path = temp_dir.path().join("fixture.zip");
        build_zip_archive(&archive_path, |writer| {
            writer
                .add_directory("pkg/", SimpleFileOptions::default())
                .unwrap();
            writer
                .start_file("pkg/bin/tool", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"#!/bin/sh\n").unwrap();
        });

        let destination = temp_dir.path().join("out");
        extract_zip(&archive_path, &destination, &AtomicBool::new(false)).unwrap();

        assert!(destination.join("pkg/bin").is_dir());
        assert!(destination.join("pkg/bin/tool").is_file());
        assert_eq!(
            std::fs::read(destination.join("pkg/bin/tool")).unwrap(),
            b"#!/bin/sh\n"
        );
    }

    #[test]
    fn zip_rejects_path_traversal_entries() {
        let temp_dir = tempdir().unwrap();
        let archive_path = temp_dir.path().join("escape.zip");
        build_raw_zip(&archive_path, "../escape.txt", b"evil");

        let destination = temp_dir.path().join("out");
        assert!(extract_zip(&archive_path, &destination, &AtomicBool::new(false)).is_err());
        assert!(!destination.join("escape.txt").exists());
        assert!(!temp_dir.path().join("escape.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn zip_extracts_symlinks_as_symlinks() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let temp_dir = tempdir().unwrap();
        let archive_path = temp_dir.path().join("fixture.zip");
        build_zip_archive(&archive_path, |writer| {
            writer
                .start_file("pkg/target", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"payload").unwrap();
            writer
                .add_symlink("pkg/link", "target", SimpleFileOptions::default())
                .unwrap();
        });

        let destination = temp_dir.path().join("out");
        extract_zip(&archive_path, &destination, &AtomicBool::new(false)).unwrap();

        let link = destination.join("pkg/link");
        let link_metadata = std::fs::symlink_metadata(&link).unwrap();
        assert!(link_metadata.file_type().is_symlink());
        assert_eq!(std::fs::read_link(&link).unwrap(), PathBuf::from("target"));
    }

    #[cfg(unix)]
    #[test]
    fn zip_preserves_unix_permissions_and_executable_helpers() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use zip::write::SimpleFileOptions;

        let temp_dir = tempdir().unwrap();
        let archive_path = temp_dir.path().join("fixture.zip");
        build_zip_archive(&archive_path, |writer| {
            writer
                .start_file(
                    "pkg/run.sh",
                    SimpleFileOptions::default().unix_permissions(0o755),
                )
                .unwrap();
            writer.write_all(b"#!/bin/sh\n").unwrap();
            writer
                .start_file(
                    "pkg/README",
                    SimpleFileOptions::default().unix_permissions(0o644),
                )
                .unwrap();
            writer.write_all(b"readme").unwrap();
        });

        let destination = temp_dir.path().join("out");
        extract_zip(&archive_path, &destination, &AtomicBool::new(false)).unwrap();

        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&destination.join("pkg/run.sh")), 0o755);
        assert_eq!(mode(&destination.join("pkg/README")), 0o644);
    }

    // --- Cancellation and stale-entry recovery ---

    /// Serves `body` over plain HTTP on an ephemeral localhost port and returns
    /// its URL. With `dribble`, the body is sent one byte at a time with a
    /// pause between bytes, so a client stays suspended mid-download until the
    /// test aborts it.
    async fn serve_archive(body: Vec<u8>, dribble: Option<Duration>, file_name: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await;
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(headers.as_bytes()).await;
            match dribble {
                Some(interval) => {
                    for byte in body {
                        let _ = socket.write_all(&[byte]).await;
                        tokio::time::sleep(interval).await;
                    }
                }
                None => {
                    let _ = socket.write_all(&body).await;
                }
            }
        });
        format!("http://{address}/{file_name}")
    }

    async fn serve_archive_without_length(body: Vec<u8>, file_name: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                .await;
            let _ = socket.write_all(&body).await;
        });
        format!("http://{address}/{file_name}")
    }

    async fn serve_stalled_archive(content_length: usize, file_name: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await;
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
            );
            let _ = socket.write_all(headers.as_bytes()).await;
            let _ = socket.write_all(b"x").await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        format!("http://{address}/{file_name}")
    }

    /// Builds a ZIP archive with `entry_count` stored (uncompressed) entries.
    /// Building stored entries is cheap in debug builds, while extracting tens
    /// of thousands of them takes seconds, so a cancellation can be observed
    /// deterministically mid-extraction.
    fn many_entries_zip(entry_count: u32) -> Vec<u8> {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let mut bytes = Vec::new();
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for index in 0..entry_count {
            writer
                .start_file(format!("pkg/f{index:05}.bin"), options)
                .unwrap();
            writer.write_all(&[index as u8; 64]).unwrap();
        }
        writer.finish().unwrap();
        bytes
    }

    #[test]
    fn zip_rejects_expanded_size_and_entry_quotas() {
        let temp_dir = tempdir().unwrap();
        let archive_path = temp_dir.path().join("fixture.zip");
        std::fs::write(&archive_path, many_entries_zip(3)).unwrap();
        let base = ArchiveLimits {
            max_download_bytes: 1024 * 1024,
            max_expanded_bytes: 128,
            max_entries: 10,
            max_files: 10,
            ..ArchiveLimits::default()
        };
        let error = extract_zip_with_limits(
            &archive_path,
            &temp_dir.path().join("out-size"),
            &AtomicBool::new(false),
            base,
        )
        .unwrap_err();
        assert!(error.to_string().contains("expanded archive size limit"));

        let error = extract_zip_with_limits(
            &archive_path,
            &temp_dir.path().join("out-entries"),
            &AtomicBool::new(false),
            ArchiveLimits {
                max_entries: 2,
                ..base
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("entry limit"));
    }

    #[test]
    fn compressed_tar_and_zip_reject_expanded_size_bombs() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use zip::write::SimpleFileOptions;

        let temp_dir = tempdir().unwrap();
        let payload = vec![0u8; 64 * 1024];
        let limits = ArchiveLimits {
            max_expanded_bytes: 1024,
            ..ArchiveLimits::default()
        };

        let tar_path = temp_dir.path().join("bomb.tar.gz");
        let encoder = GzEncoder::new(File::create(&tar_path).unwrap(), Compression::best());
        let mut tar = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_path("pkg/payload.bin").unwrap();
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, payload.as_slice()).unwrap();
        tar.into_inner().unwrap().finish().unwrap();

        let error = extract_archive_blocking(
            &tar_path,
            &temp_dir.path().join("tar-out"),
            &AtomicBool::new(false),
            limits,
        )
        .unwrap_err();
        assert!(error.to_string().contains("expanded archive size limit"));

        let zip_path = temp_dir.path().join("bomb.zip");
        build_zip_archive(&zip_path, |writer| {
            writer
                .start_file(
                    "pkg/payload.bin",
                    SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Deflated),
                )
                .unwrap();
            writer.write_all(&payload).unwrap();
        });
        let error = extract_zip_with_limits(
            &zip_path,
            &temp_dir.path().join("zip-out"),
            &AtomicBool::new(false),
            limits,
        )
        .unwrap_err();
        assert!(error.to_string().contains("expanded archive size limit"));
    }

    #[tokio::test]
    async fn download_rejects_oversized_response_before_writing() {
        let temp_dir = tempdir().unwrap();
        let body = vec![0x42; 32];
        let digest = hex_encode(Sha256::digest(&body).as_slice());
        let url = serve_archive(body, None, "payload.zip").await;
        let target = BinaryTarget {
            archive: url,
            cmd: "tool".to_string(),
            sha256: Some(digest),
            args: None,
            env: None,
        };
        let error = download_archive_with_limits(
            &target,
            temp_dir.path(),
            ArchiveLimits {
                max_download_bytes: 16,
                ..ArchiveLimits::default()
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("download limit"));
        assert!(!temp_dir.path().join("payload.zip").exists());
    }

    #[tokio::test]
    async fn download_enforces_limit_without_content_length() {
        let temp_dir = tempdir().unwrap();
        let body = vec![0x42; 32];
        let digest = hex_encode(Sha256::digest(&body).as_slice());
        let target = BinaryTarget {
            archive: serve_archive_without_length(body, "payload.zip").await,
            cmd: "tool".to_string(),
            sha256: Some(digest),
            args: None,
            env: None,
        };

        let error = download_archive_with_limits(
            &target,
            temp_dir.path(),
            ArchiveLimits {
                max_download_bytes: 16,
                ..ArchiveLimits::default()
            },
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("download limit"));
        assert!(!temp_dir.path().join("payload.zip").exists());
    }

    #[tokio::test]
    async fn download_read_timeout_removes_partial_file() {
        let temp_dir = tempdir().unwrap();
        let target = BinaryTarget {
            archive: serve_stalled_archive(32, "payload.zip").await,
            cmd: "tool".to_string(),
            sha256: Some(hex_encode(Sha256::digest([0u8; 32]).as_slice())),
            args: None,
            env: None,
        };

        let error = download_archive_with_limits(
            &target,
            temp_dir.path(),
            ArchiveLimits {
                read_timeout: Duration::from_millis(50),
                total_timeout: Duration::from_secs(2),
                ..ArchiveLimits::default()
            },
        )
        .await
        .unwrap_err();

        assert!(
            format!("{error:#}").contains("failed to read archive response"),
            "unexpected timeout error: {error:#}"
        );
        assert!(!temp_dir.path().join("payload.zip").exists());
    }

    /// Dot-prefixed entry names directly inside `dir` (staging/backup work
    /// directories left behind by interrupted installs).
    async fn work_dirs_in(dir: &Path) -> Vec<String> {
        let mut names = Vec::new();
        let Ok(mut entries) = fs::read_dir(dir).await else {
            return names;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                names.push(name);
            }
        }
        names
    }

    /// Waits until the in-flight install has started writing extracted entries
    /// into its staging directory, then returns, so a test can land a
    /// cancellation mid-extraction regardless of machine speed.
    async fn wait_for_extraction_started(parent_dir: &Path) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let Ok(mut entries) = fs::read_dir(parent_dir).await else {
                continue;
            };
            while let Ok(Some(entry)) = entries.next_entry().await {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.starts_with('.') {
                    continue;
                }
                let Ok(mut extracted_entries) = fs::read_dir(entry.path().join("extracted")).await
                else {
                    continue;
                };
                if let Ok(Some(_)) = extracted_entries.next_entry().await {
                    return;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "extraction never started"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn binary_test_agent(version: &str, archive_url: &str) -> RegistryAgent {
        let binary = BinaryDistribution {
            linux_x86_64: Some(BinaryTarget {
                archive: archive_url.to_string(),
                cmd: "./bin/demo".to_string(),
                sha256: None,
                args: None,
                env: None,
            }),
            ..BinaryDistribution::default()
        };
        RegistryAgent {
            id: "demo".to_string(),
            name: "Demo".to_string(),
            version: version.to_string(),
            description: "Demo agent".to_string(),
            repository: None,
            website: None,
            authors: vec!["ACP".to_string()],
            license: "MIT".to_string(),
            icon: None,
            distribution: AgentDistribution {
                binary: Some(binary),
                npx: None,
                uvx: None,
            },
        }
    }

    #[tokio::test]
    async fn malformed_digest_is_rejected_before_cache_path_creation() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache");
        let platform = Platform::LinuxX86_64;
        let agent = binary_test_agent("1.0.0", "not a valid URL");
        let mut target = agent
            .distribution
            .binary
            .as_ref()
            .unwrap()
            .for_platform(platform)
            .unwrap()
            .clone();
        target.sha256 = Some("../../not-a-digest".to_string());

        let error = cache_binary_target_in_mode(&cache_root, &agent, platform, &target, false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("invalid sha256 checksum"));
        assert!(!cache_root.exists());
    }

    #[tokio::test]
    async fn cancelled_download_removes_the_staging_directory() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let platform = Platform::LinuxX86_64;
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", platform);

        // The archive dribbles out over minutes, so the install is guaranteed
        // to still be awaiting the download when the task is aborted.
        let body = vec![0x42; 64 * 1024];
        let digest = hex_encode(Sha256::digest(&body).as_slice());
        let url = serve_archive(body, Some(Duration::from_millis(25)), "agent.tar.gz").await;
        let agent = binary_test_agent("1.0.0", &url);
        let mut target = agent
            .distribution
            .binary
            .as_ref()
            .unwrap()
            .for_platform(platform)
            .unwrap()
            .clone();
        target.sha256 = Some(digest);

        let root = cache_root.clone();
        let task = tokio::spawn(async move {
            cache_binary_target_in_mode(&root, &agent, platform, &target, false).await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        task.abort();
        assert!(task.await.is_err(), "install should have been cancelled");

        assert!(!paths.cache_dir.exists(), "no cache may be promoted");
        assert_eq!(
            work_dirs_in(&paths.parent_dir).await,
            Vec::<String>::new(),
            "staging directory must be removed on cancellation"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_extraction_removes_the_staging_directory() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let platform = Platform::LinuxX86_64;
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", platform);

        // Stay below the production entry quota while still making extraction
        // long enough for cancellation to land between entries.
        let body = many_entries_zip(8_000);
        let digest = hex_encode(Sha256::digest(&body).as_slice());
        let url = serve_archive(body, None, "agent.zip").await;
        let agent = binary_test_agent("1.0.0", &url);
        let mut target = agent
            .distribution
            .binary
            .as_ref()
            .unwrap()
            .for_platform(platform)
            .unwrap()
            .clone();
        target.sha256 = Some(digest);

        let root = cache_root.clone();
        let task = tokio::spawn(async move {
            cache_binary_target_in_mode(&root, &agent, platform, &target, false).await
        });
        wait_for_extraction_started(&paths.parent_dir).await;
        task.abort();
        assert!(task.await.is_err(), "install should have been cancelled");

        // The detached blocking extraction keeps running until its next write
        // fails against the removed directory; give it a moment to wind down
        // before asserting the staging directory is gone.
        for _ in 0..100 {
            if work_dirs_in(&paths.parent_dir).await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!paths.cache_dir.exists(), "no cache may be promoted");
        assert_eq!(
            work_dirs_in(&paths.parent_dir).await,
            Vec::<String>::new(),
            "staging directory must be removed on cancellation"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_publication_wait_removes_prepared_staging() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths_with_digest(
            &cache_root,
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            &"a".repeat(64),
        );
        fs::create_dir_all(&paths.parent_dir).await.unwrap();
        let staging_path = paths.parent_dir.join(".prepared-staging-cancel");
        fs::create_dir_all(&staging_path).await.unwrap();
        fs::write(staging_path.join("payload"), b"prepared")
            .await
            .unwrap();

        // Hold a reader so the publisher blocks while waiting for the exact
        // use-write lease acquired by the production path.
        let held_reader = acquire_binary_cache_use_read_lock(&paths).await.unwrap();
        let publication_lock = Arc::new(acquire_binary_cache_lock(&paths).await.unwrap());
        let task_paths = paths.clone();
        let task_staging = PreparedStaging::new(staging_path.clone(), publication_lock);
        let task = tokio::spawn(async move {
            let _writer = acquire_binary_cache_use_write_lock(&task_paths)
                .await
                .unwrap();
            let expected = BinaryCacheMetadata::new(
                "demo",
                "1.0.0",
                Platform::LinuxX86_64,
                "https://example.com/demo.tar.gz",
                "./demo",
                Some(&"a".repeat(64)),
            );
            promote_prepared_cache(task_staging, &task_paths, &expected, false, None).await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        task.abort();
        assert!(task.await.is_err(), "publication wait should be cancelled");
        drop(held_reader);

        for _ in 0..100 {
            if !staging_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !staging_path.exists(),
            "prepared staging must be cleaned up"
        );
        tokio::time::timeout(
            Duration::from_secs(2),
            acquire_binary_cache_use_write_lock(&paths),
        )
        .await
        .expect("cancelled publication must release its use lock")
        .unwrap();
    }

    #[tokio::test]
    async fn startup_sweep_removes_stale_staging_and_backup_directories() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);

        // A completed cache entry must survive the sweep.
        fs::create_dir_all(&paths.extracted_dir).await.unwrap();
        fs::write(&paths.metadata_path, b"{}").await.unwrap();

        // Work directories abandoned by installs that were killed mid-flight.
        let stale_staging = paths.parent_dir.join(".1.0.0-staging-123-456");
        let stale_backup = paths.parent_dir.join(".1.0.0-backup-123-456");
        fs::create_dir_all(stale_staging.join("extracted"))
            .await
            .unwrap();
        fs::create_dir_all(&stale_backup).await.unwrap();

        let removed = clean_stale_staging_entries_in(&cache_root).await;

        assert_eq!(removed, 2);
        assert!(!stale_staging.exists());
        assert!(!stale_backup.exists());
        assert!(paths.cache_dir.exists());
        assert!(paths.metadata_path.exists());
    }

    #[tokio::test]
    async fn startup_sweep_restores_backup_when_final_cache_is_missing() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        let backup = paths.parent_dir.join(".1.0.0-backup-crashed");
        fs::create_dir_all(backup.join(EXTRACTED_DIR_NAME))
            .await
            .unwrap();
        fs::write(backup.join(METADATA_FILE_NAME), b"{\"recovered\":true}")
            .await
            .unwrap();

        let removed = clean_stale_staging_entries_in(&cache_root).await;

        assert_eq!(removed, 0);
        assert!(paths.cache_dir.exists());
        assert!(!backup.exists());
        assert_eq!(
            fs::read(paths.metadata_path).await.unwrap(),
            b"{\"recovered\":true}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_sweep_skips_active_staging_lock() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        fs::create_dir_all(&paths.parent_dir).await.unwrap();
        let active_staging = paths.parent_dir.join(".1.0.0-staging-active");
        fs::create_dir_all(&active_staging).await.unwrap();

        let lock = acquire_binary_cache_lock(&paths).await.unwrap();
        let removed = tokio::time::timeout(
            Duration::from_secs(1),
            clean_stale_staging_entries_in(&cache_root),
        )
        .await
        .expect("startup sweep must not wait for an active installer");
        assert_eq!(removed, 0);
        assert!(active_staging.exists());

        drop(lock);
        assert_eq!(clean_stale_staging_entries_in(&cache_root).await, 1);
        assert!(!active_staging.exists());
    }
}
