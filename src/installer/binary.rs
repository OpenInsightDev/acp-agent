use std::fs::File;
use std::future::Future;
use std::io;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context as TaskContext, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use serde_json::to_vec_pretty;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio::fs;
use zip::ZipArchive;

use crate::installer::cache::{
    AGENTS_DIR, BinaryCacheMetadata, BinaryCachePaths, EXTRACTED_DIR_NAME, METADATA_FILE_NAME,
    binary_cache_paths, cache_root_dir, platform_cache_key, safe_path_component,
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

/// A validated binary distribution stored in the local cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedBinary {
    /// Resolved executable path within the extracted payload.
    pub executable_path: PathBuf,
    /// Directory containing the extracted payload.
    pub extracted_dir: PathBuf,
    /// Stable cache directory that owns the extracted payload.
    pub cache_dir: PathBuf,
}

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

/// Rebuilds a cached target even when its registry metadata has not changed.
/// The existing entry remains available until the replacement is ready.
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
    let paths = binary_cache_paths(root_dir, &agent.id, &agent.version, platform);
    let expected = BinaryCacheMetadata::new(
        &agent.id,
        &agent.version,
        platform,
        &target.archive,
        &target.cmd,
        target.sha256.as_deref(),
    );

    if !force_refresh && let Some(prepared) = validate_cached_binary(&paths, &expected).await? {
        return Ok(prepared);
    }

    fs::create_dir_all(&paths.parent_dir)
        .await
        .with_context(|| format!("failed to create {}", paths.parent_dir.display()))?;

    // Stage the new cache in a `tempfile` guard so the payload is removed no
    // matter how this future ends: an explicit error, a panic, or a
    // cancellation while awaiting the download, extraction, or metadata
    // write. The staged payload is renamed into the stable cache directory
    // only after it has been fully prepared.
    let staging = tempfile::Builder::new()
        .prefix(&format!(
            ".{}-staging-",
            safe_path_component(&agent.version)
        ))
        .tempdir_in(&paths.parent_dir)
        .with_context(|| {
            format!(
                "failed to create staging directory in {}",
                paths.parent_dir.display()
            )
        })?;

    prepare_staging_directory(staging.path(), target, &expected).await?;

    let cached = promote_staged_cache(staging.path(), &paths, &expected, force_refresh).await?;
    // Promotion moved (or removed) the staged directory, so the guard's path
    // no longer exists; keep it from attempting a redundant cleanup.
    let _ = staging.keep();
    Ok(cached)
}

/// Moves a fully prepared staging directory into the stable cache location.
///
/// The caller owns the staging directory through an RAII guard; on any error
/// the guard removes whatever remains at `staging_dir`. This function only
/// removes the staging directory itself on the one success path where it is
/// abandoned: an existing valid cache is kept instead of the staged one.
async fn promote_staged_cache(
    staging_dir: &Path,
    paths: &BinaryCachePaths,
    expected: &BinaryCacheMetadata,
    replace_existing: bool,
) -> Result<CachedBinary> {
    match fs::rename(staging_dir, &paths.cache_dir).await {
        Ok(()) => {
            if let Some(cached) = validate_cached_binary(paths, expected).await? {
                return Ok(cached);
            }
            bail!(
                "cache directory {} was created, but validation still failed",
                paths.cache_dir.display()
            );
        }
        Err(rename_error) => {
            if !replace_existing
                && let Some(cached) = validate_cached_binary(paths, expected).await?
            {
                cleanup_dir(staging_dir).await;
                return Ok(cached);
            }

            if fs::try_exists(&paths.cache_dir)
                .await
                .with_context(|| format!("failed to inspect {}", paths.cache_dir.display()))?
            {
                let backup_dir = paths
                    .parent_dir
                    .join(unique_backup_dir_name(&paths.cache_dir));
                if let Err(backup_error) = fs::rename(&paths.cache_dir, &backup_dir).await {
                    return Err(backup_error).with_context(|| {
                        format!(
                            "failed to preserve existing cache directory {} before replacement",
                            paths.cache_dir.display()
                        )
                    });
                }

                if let Err(promote_error) = fs::rename(staging_dir, &paths.cache_dir).await {
                    let restore_result = fs::rename(&backup_dir, &paths.cache_dir).await;
                    return match restore_result {
                        Ok(()) => Err(promote_error).with_context(|| {
                            format!(
                                "failed to promote staged cache {} to {}; restored the previous cache",
                                staging_dir.display(),
                                paths.cache_dir.display()
                            )
                        }),
                        Err(restore_error) => Err(promote_error).with_context(|| {
                            format!(
                                "failed to promote staged cache {} to {} and failed to restore {}: {}",
                                staging_dir.display(),
                                paths.cache_dir.display(),
                                backup_dir.display(),
                                restore_error
                            )
                        }),
                    };
                }

                if let Some(cached) = validate_cached_binary(paths, expected).await? {
                    cleanup_dir(&backup_dir).await;
                    return Ok(cached);
                }

                cleanup_dir(&paths.cache_dir).await;
                if let Err(restore_error) = fs::rename(&backup_dir, &paths.cache_dir).await {
                    return Err(restore_error).with_context(|| {
                        format!(
                            "replacement cache validation failed and the previous cache at {} could not be restored",
                            backup_dir.display()
                        )
                    });
                }
                bail!(
                    "replacement cache {} failed validation; restored the previous cache",
                    paths.cache_dir.display()
                );
            }

            Err(rename_error).with_context(|| {
                format!(
                    "failed to promote staged cache {} to {}",
                    staging_dir.display(),
                    paths.cache_dir.display()
                )
            })
        }
    }
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
                if version_entry.file_name().to_string_lossy().starts_with('.')
                    && fs::remove_dir_all(version_entry.path()).await.is_ok()
                {
                    removed += 1;
                }
            }
        }
    }
    removed
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
    let mut name = path.as_os_str().to_os_string();
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
    let url = reqwest::Url::parse(&target.archive)
        .with_context(|| format!("invalid archive URL: {}", target.archive))?;
    let archive_name = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|segment| !segment.is_empty())
        .unwrap_or("download.bin");
    let destination = temp_dir.join(archive_name);

    let response = reqwest::get(url)
        .await
        .with_context(|| format!("failed to download archive from {}", target.archive))?;
    let response = response
        .error_for_status()
        .with_context(|| format!("failed to download archive from {}", target.archive))?;
    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("failed to read archive response from {}", target.archive))?;
    verify_sha256(bytes.as_ref(), target.sha256.as_deref())
        .with_context(|| format!("integrity check failed for archive {}", target.archive))?;
    fs::write(&destination, bytes.as_ref())
        .await
        .with_context(|| {
            format!(
                "failed to write downloaded archive to {}",
                destination.display()
            )
        })?;
    Ok(destination)
}

/// Verifies downloaded bytes against the registry-declared SHA-256 digest.
///
/// A `None` digest means the registry published no checksum for this target;
/// the download is accepted without verification so older entries keep working.
fn verify_sha256(bytes: &[u8], expected: Option<&str>) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
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

pub(crate) async fn extract_archive(archive_path: PathBuf, destination: PathBuf) -> Result<()> {
    let cancel = Arc::new(AtomicBool::new(false));
    let task_cancel = Arc::clone(&cancel);
    let handle = tokio::task::spawn_blocking(move || {
        extract_archive_blocking(&archive_path, &destination, &task_cancel)
    });
    // Dropping this future (an aborted or cancelled install) sets the flag, so
    // the detached blocking extraction stops before the staging guard removes
    // the directory instead of racing it.
    CancellableExtraction { handle, cancel }.await
}

/// [`spawn_blocking`] join that signals a cancellation flag when dropped.
///
/// [`tokio::task::spawn_blocking`] tasks cannot be cancelled directly: dropping
/// the [`JoinHandle`] merely detaches them, so an aborted install would leave a
/// writer running against a directory the staging guard is deleting. Setting
/// the flag from `Drop` lets the blocking extraction cooperate with the
/// cancellation before the guard's removal runs.
struct CancellableExtraction {
    handle: tokio::task::JoinHandle<Result<()>>,
    cancel: Arc<AtomicBool>,
}

impl Future for CancellableExtraction {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.handle).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(result)) => Poll::Ready(result),
            Poll::Ready(Err(error)) => Poll::Ready(Err(anyhow!("extraction task failed: {error}"))),
        }
    }
}

impl Drop for CancellableExtraction {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

fn extract_archive_blocking(
    archive_path: &Path,
    destination: &Path,
    cancel: &AtomicBool,
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
        return extract_zip(archive_path, destination, cancel);
    }

    if file_name.ends_with(".tar.gz") || file_name.ends_with(".tgz") {
        let file = File::open(archive_path)
            .with_context(|| format!("failed to open archive {}", archive_path.display()))?;
        let decoder = GzDecoder::new(file);
        return extract_tar(decoder, destination, cancel);
    }

    if file_name.ends_with(".tar.bz2") || file_name.ends_with(".tbz2") {
        let file = File::open(archive_path)
            .with_context(|| format!("failed to open archive {}", archive_path.display()))?;
        let decoder = BzDecoder::new(file);
        return extract_tar(decoder, destination, cancel);
    }

    let file_name = archive_path
        .file_name()
        .ok_or_else(|| anyhow!("unsupported archive format for {}", archive_path.display()))?;
    let fallback_path = destination.join(file_name);
    std::fs::copy(archive_path, &fallback_path).with_context(|| {
        format!(
            "failed to copy archive {} to {}",
            archive_path.display(),
            fallback_path.display()
        )
    })?;
    Ok(())
}

/// Extracts a tar archive entry by entry, checking the cancellation flag
/// between entries so an aborted install stops writing promptly.
fn extract_tar<R: io::Read>(reader: R, destination: &Path, cancel: &AtomicBool) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    let entries = archive.entries().with_context(|| {
        format!(
            "failed to read archive entries for {}",
            destination.display()
        )
    })?;
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
        entry
            .unpack(destination)
            .with_context(|| format!("failed to unpack archive into {}", destination.display()))?;
    }
    Ok(())
}

fn extract_zip(archive_path: &Path, destination: &Path, cancel: &AtomicBool) -> Result<()> {
    let file = File::open(archive_path)
        .with_context(|| format!("failed to open archive {}", archive_path.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("failed to read ZIP archive {}", archive_path.display()))?;

    // Modes are applied in a second pass (children first) so a read-only
    // directory entry cannot prevent its own contents from being extracted.
    #[cfg(unix)]
    let mut unix_modes: Vec<(PathBuf, u32)> = Vec::new();

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
        let outpath = destination.join(enclosed);

        if entry.is_dir() {
            std::fs::create_dir_all(&outpath)
                .with_context(|| format!("failed to create directory {}", outpath.display()))?;
            continue;
        }

        if entry.is_symlink() {
            extract_zip_symlink(&mut entry, &outpath)?;
            continue;
        }

        if let Some(parent) = outpath.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let mut outfile = File::create(&outpath)
            .with_context(|| format!("failed to create {}", outpath.display()))?;
        io::copy(&mut entry, &mut outfile)
            .with_context(|| format!("failed to write {}", outpath.display()))?;

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
) -> Result<()> {
    let mut target = Vec::with_capacity(entry.size() as usize);
    entry
        .read_to_end(&mut target)
        .with_context(|| format!("failed to read symlink target for {}", outpath.display()))?;
    let target = String::from_utf8(target).with_context(|| {
        format!(
            "symlink target for {} is not valid UTF-8",
            outpath.display()
        )
    })?;

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
    staging_dir: &Path,
    target: &BinaryTarget,
    metadata: &BinaryCacheMetadata,
) -> Result<()> {
    let archive_path = download_archive(target, staging_dir).await?;
    let extracted_dir = staging_dir.join(EXTRACTED_DIR_NAME);
    fs::create_dir_all(&extracted_dir)
        .await
        .with_context(|| format!("failed to create {}", extracted_dir.display()))?;
    extract_archive(archive_path, extracted_dir.clone()).await?;

    let executable_path = resolve_cmd_path(&extracted_dir, &target.cmd)?;
    let file_metadata = fs::metadata(&executable_path).await;
    if file_metadata
        .as_ref()
        .map(|metadata| !metadata.is_file())
        .unwrap_or(true)
    {
        bail!(
            "downloaded {}, but could not find \"{}\" at {}",
            target.archive,
            target.cmd,
            executable_path.display()
        );
    }

    make_executable(&executable_path)
        .await
        .with_context(|| format!("failed to mark {} executable", executable_path.display()))?;

    let metadata_path = staging_dir.join(METADATA_FILE_NAME);
    let metadata_bytes =
        to_vec_pretty(metadata).context("failed to encode cached binary metadata")?;
    fs::write(&metadata_path, metadata_bytes)
        .await
        .with_context(|| format!("failed to write {}", metadata_path.display()))?;

    Ok(())
}

async fn validate_cached_binary(
    paths: &BinaryCachePaths,
    expected: &BinaryCacheMetadata,
) -> Result<Option<CachedBinary>> {
    if !fs::try_exists(&paths.metadata_path)
        .await
        .with_context(|| format!("failed to inspect {}", paths.metadata_path.display()))?
    {
        return Ok(None);
    }

    let metadata_bytes = fs::read(&paths.metadata_path)
        .await
        .with_context(|| format!("failed to read {}", paths.metadata_path.display()))?;
    let metadata: BinaryCacheMetadata = match serde_json::from_slice(&metadata_bytes) {
        Ok(metadata) => metadata,
        Err(_) => {
            cleanup_dir(&paths.cache_dir).await;
            return Ok(None);
        }
    };
    if &metadata != expected {
        return Ok(None);
    }

    let executable_path = match resolve_cmd_path(&paths.extracted_dir, &metadata.cmd) {
        Ok(path) => path,
        Err(_) => {
            cleanup_dir(&paths.cache_dir).await;
            return Ok(None);
        }
    };
    let file_metadata = fs::metadata(&executable_path).await;
    if file_metadata
        .as_ref()
        .map(|metadata| !metadata.is_file())
        .unwrap_or(true)
    {
        return Ok(None);
    }

    Ok(Some(CachedBinary {
        executable_path,
        extracted_dir: paths.extracted_dir.clone(),
        cache_dir: paths.cache_dir.clone(),
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
    use crate::registry::{AgentDistribution, BinaryDistribution};
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn accepts_download_without_declared_sha256() {
        verify_sha256(b"payload", None).expect("missing digest should be accepted");
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
        fs::write(&paths.metadata_path, serde_json::to_vec(&metadata).unwrap())
            .await
            .unwrap();
        let executable_path = paths.extracted_dir.join("bin").join("demo");
        fs::create_dir_all(executable_path.parent().unwrap())
            .await
            .unwrap();
        fs::write(&executable_path, b"#!/bin/sh\n").await.unwrap();

        let prepared = validate_cached_binary(&paths, &metadata).await.unwrap();
        assert_eq!(
            prepared.unwrap(),
            CachedBinary {
                executable_path,
                extracted_dir: paths.extracted_dir,
                cache_dir: paths.cache_dir,
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
    async fn corrupted_metadata_is_treated_as_cache_miss_and_removed() {
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
        assert!(!fs::try_exists(&paths.cache_dir).await.unwrap());
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
            promote_staged_cache(&missing_staging, &paths, &replacement, true)
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

        promote_staged_cache(&staging_dir, &paths, &metadata, true)
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
    async fn cancelled_download_removes_the_staging_directory() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let platform = Platform::LinuxX86_64;
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", platform);

        // The archive dribbles out over minutes, so the install is guaranteed
        // to still be awaiting the download when the task is aborted.
        let url = serve_archive(
            vec![0x42; 64 * 1024],
            Some(Duration::from_millis(25)),
            "agent.tar.gz",
        )
        .await;
        let agent = binary_test_agent("1.0.0", &url);
        let target = agent
            .distribution
            .binary
            .as_ref()
            .unwrap()
            .for_platform(platform)
            .unwrap()
            .clone();

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

        // A 30k-entry stored ZIP downloads in a few milliseconds (the archive
        // is a few megabytes) but takes seconds to extract, so the abort lands
        // while the blocking extraction is still writing entries.
        let url = serve_archive(many_entries_zip(30_000), None, "agent.zip").await;
        let agent = binary_test_agent("1.0.0", &url);
        let target = agent
            .distribution
            .binary
            .as_ref()
            .unwrap()
            .for_platform(platform)
            .unwrap()
            .clone();

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
}
