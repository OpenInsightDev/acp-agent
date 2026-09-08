use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
#[cfg(test)]
use tokio::fs;

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use super::archive::check_extraction_cancelled;
use super::download::hex_encode;
use super::paths::resolve_cmd_path;
use crate::installer::cache::{BinaryCacheLock, BinaryCacheMetadata, BinaryCachePaths};

use super::CachedBinary;
pub(crate) fn hash_file_sha256_blocking(path: &Path, cancel: &AtomicBool) -> Result<String> {
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
pub(crate) async fn hash_payload_sha256(path: &Path) -> Result<String> {
    let path = path.to_path_buf();
    let cancel = Arc::new(AtomicBool::new(false));
    let task_cancel = Arc::clone(&cancel);
    tokio::task::spawn_blocking(move || hash_payload_sha256_blocking(&path, &task_cancel))
        .await
        .map_err(|error| anyhow!("payload hash task failed: {error}"))?
}

pub(crate) fn hash_payload_sha256_blocking(path: &Path, cancel: &AtomicBool) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"acp-agent-payload-tree-v2\0");
    hash_payload_entry(path, Path::new(""), &mut hasher, cancel)?;
    Ok(hex_encode(hasher.finalize().as_slice()))
}

pub(super) fn hash_length_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

pub(super) fn path_hash_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().to_vec()
}

pub(crate) fn hash_payload_entry(
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

pub(crate) fn make_executable_blocking(path: &Path) -> Result<(), io::Error> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o100);
    std::fs::set_permissions(path, permissions)
}

#[cfg(test)]
pub(crate) async fn make_executable(path: &Path) -> Result<(), io::Error> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path).await?.permissions();
    permissions.set_mode(permissions.mode() | 0o100);
    fs::set_permissions(path, permissions).await
}
#[cfg(test)]
pub(crate) async fn validate_cached_binary(
    paths: &BinaryCachePaths,
    expected: &BinaryCacheMetadata,
) -> Result<Option<CachedBinary>> {
    validate_cached_binary_with_lease(paths, expected, None).await
}

pub(crate) async fn validate_cached_binary_with_lease(
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

pub(crate) fn validate_cached_binary_blocking(
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
