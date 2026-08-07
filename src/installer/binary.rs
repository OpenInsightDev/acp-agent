use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use serde_json::to_vec_pretty;
use sha2::{Digest, Sha256};
use tokio::fs;
use zip::ZipArchive;

use crate::installer::cache::{
    BinaryCacheMetadata, BinaryCachePaths, EXTRACTED_DIR_NAME, METADATA_FILE_NAME,
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

/// Serializes appends to the shared install log.
///
/// Concurrent installs (multiple agents at once) write to one log file, and
/// the truncation path rewrites the whole file. Without a lock, two racing
/// appends could interleave or silently drop lines, so every append (and its
/// potential truncate-and-rewrite) runs under this mutex.
static INSTALL_LOG_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

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
    record_install_log(agent, platform, &result);
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
    record_install_log_in(root_dir, agent, platform, &result);
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
        make_executable(&prepared.executable_path)
            .await
            .with_context(|| {
                format!(
                    "failed to mark {} executable",
                    prepared.executable_path.display()
                )
            })?;
        return Ok(prepared);
    }

    fs::create_dir_all(&paths.parent_dir)
        .await
        .with_context(|| format!("failed to create {}", paths.parent_dir.display()))?;

    let staging_dir = paths
        .parent_dir
        .join(unique_staging_dir_name(&agent.version));
    fs::create_dir_all(&staging_dir)
        .await
        .with_context(|| format!("failed to create {}", staging_dir.display()))?;

    let staged = prepare_staging_directory(&staging_dir, target, &expected).await;
    match staged {
        Ok(()) => {}
        Err(error) => {
            cleanup_dir(&staging_dir).await;
            return Err(error);
        }
    }

    promote_staged_cache(&staging_dir, &paths, &expected, force_refresh).await
}

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

            if try_exists(&paths.cache_dir).await? {
                let backup_dir = paths
                    .parent_dir
                    .join(unique_backup_dir_name(&paths.cache_dir));
                if let Err(backup_error) = fs::rename(&paths.cache_dir, &backup_dir).await {
                    cleanup_dir(staging_dir).await;
                    return Err(backup_error).with_context(|| {
                        format!(
                            "failed to preserve existing cache directory {} before replacement",
                            paths.cache_dir.display()
                        )
                    });
                }

                if let Err(promote_error) = fs::rename(staging_dir, &paths.cache_dir).await {
                    let restore_result = fs::rename(&backup_dir, &paths.cache_dir).await;
                    cleanup_dir(staging_dir).await;
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

            cleanup_dir(staging_dir).await;
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

/// Appends one line per binary install attempt to `agent-install.log`.
///
/// Successes are logged too (cache hits included): a `ready` line is the only
/// way to confirm from outside the shell-less container which agent versions
/// are present in `/cache`; failures carry the full error chain.
fn record_install_log(agent: &RegistryAgent, platform: Platform, result: &Result<CachedBinary>) {
    let Ok(root_dir) = cache_root_dir() else {
        return;
    };
    record_install_log_in(&root_dir, agent, platform, result);
}

fn record_install_log_in(
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
    append_install_log(
        &root_dir.join(INSTALL_LOG_FILE_NAME),
        &format!("[{}] {outcome}\n", utc_timestamp()),
    );
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

    let lock = INSTALL_LOG_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
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

/// Keeps only the most recent tail so the append-only log stays bounded in a
/// long-lived cache volume; the leading partial line is dropped so retained
/// lines stay complete.
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
    std::fs::write(path, rewritten)
}

/// UTC timestamp in `YYYY-MM-DDTHH:MM:SSZ` form, without extra dependencies.
fn utc_timestamp() -> String {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let days = (since_epoch.as_secs() / 86_400) as i64;
    let secs_of_day = since_epoch.as_secs() % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

/// Converts days since the Unix epoch to a `(year, month, day)` civil date
/// using Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (
        if month <= 2 { year + 1 } else { year },
        month as u32,
        day as u32,
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
    tokio::task::spawn_blocking(move || extract_archive_blocking(&archive_path, &destination))
        .await
        .context("blocking task failed while extracting archive")?
}

fn extract_archive_blocking(archive_path: &Path, destination: &Path) -> Result<()> {
    let file_name = archive_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if file_name.ends_with(".zip") {
        return extract_zip(archive_path, destination);
    }

    if file_name.ends_with(".tar.gz") || file_name.ends_with(".tgz") {
        let file = File::open(archive_path)
            .with_context(|| format!("failed to open archive {}", archive_path.display()))?;
        let decoder = GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        archive
            .unpack(destination)
            .with_context(|| format!("failed to unpack archive into {}", destination.display()))?;
        return Ok(());
    }

    if file_name.ends_with(".tar.bz2") || file_name.ends_with(".tbz2") {
        let file = File::open(archive_path)
            .with_context(|| format!("failed to open archive {}", archive_path.display()))?;
        let decoder = BzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        archive
            .unpack(destination)
            .with_context(|| format!("failed to unpack archive into {}", destination.display()))?;
        return Ok(());
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

fn extract_zip(archive_path: &Path, destination: &Path) -> Result<()> {
    let file = File::open(archive_path)
        .with_context(|| format!("failed to open archive {}", archive_path.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("failed to read ZIP archive {}", archive_path.display()))?;

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("failed to read ZIP entry {index}"))?;
        let enclosed = entry
            .enclosed_name()
            .ok_or_else(|| anyhow!("archive contains unsafe path: {}", entry.name()))?;
        let output_path = destination.join(enclosed);

        if entry.name().ends_with('/') {
            std::fs::create_dir_all(&output_path)
                .with_context(|| format!("failed to create {}", output_path.display()))?;
            continue;
        }

        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let mut output = File::create(&output_path)
            .with_context(|| format!("failed to create {}", output_path.display()))?;
        io::copy(&mut entry, &mut output)
            .with_context(|| format!("failed to extract {}", output_path.display()))?;
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
        permissions.set_mode(0o755);
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
    if !try_exists(&paths.metadata_path).await? {
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

async fn try_exists(path: &Path) -> Result<bool> {
    match fs::metadata(path).await {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

async fn cleanup_dir(path: &Path) {
    let _ = fs::remove_dir_all(path).await;
}

fn unique_staging_dir_name(agent_version: &str) -> String {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!(
        ".{}-staging-{pid}-{nanos}",
        safe_path_component(agent_version)
    )
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
        assert!(!try_exists(&paths.cache_dir).await.unwrap());
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

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        assert_eq!(civil_from_days(20_668), (2026, 8, 3));
    }
}
