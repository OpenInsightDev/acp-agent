use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use serde_json::to_vec_pretty;
use tokio::fs;
use zip::ZipArchive;

use crate::installer::cache::{
    BinaryCacheMetadata, BinaryCachePaths, EXTRACTED_DIR_NAME, METADATA_FILE_NAME,
    binary_cache_paths, cache_root_dir, safe_path_component,
};
use crate::registry::{BinaryTarget, Platform, RegistryAgent};
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
pub async fn cache_binary_target(
    agent: &RegistryAgent,
    platform: Platform,
    target: &BinaryTarget,
) -> Result<CachedBinary> {
    let root_dir = cache_root_dir()?;
    let paths = binary_cache_paths(&root_dir, &agent.id, &agent.version, platform);
    let expected = BinaryCacheMetadata::new(
        &agent.id,
        &agent.version,
        platform,
        &target.archive,
        &target.cmd,
    );

    if let Some(prepared) = validate_cached_binary(&paths, &expected).await? {
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

    match fs::rename(&staging_dir, &paths.cache_dir).await {
        Ok(()) => {
            if let Some(cached) = validate_cached_binary(&paths, &expected).await? {
                return Ok(cached);
            }
            bail!(
                "cache directory {} was created, but validation still failed",
                paths.cache_dir.display()
            );
        }
        Err(rename_error) => {
            if let Some(cached) = validate_cached_binary(&paths, &expected).await? {
                cleanup_dir(&staging_dir).await;
                return Ok(cached);
            }

            if try_exists(&paths.cache_dir).await? {
                if let Err(remove_error) = fs::remove_dir_all(&paths.cache_dir).await {
                    cleanup_dir(&staging_dir).await;
                    return Err(remove_error).with_context(|| {
                        format!(
                            "failed to replace invalid cache directory {}",
                            paths.cache_dir.display()
                        )
                    });
                }
                if let Err(rename_error) = fs::rename(&staging_dir, &paths.cache_dir).await {
                    cleanup_dir(&staging_dir).await;
                    return Err(rename_error).with_context(|| {
                        format!(
                            "failed to promote staged cache {} to {}",
                            staging_dir.display(),
                            paths.cache_dir.display()
                        )
                    });
                }
                if let Some(cached) = validate_cached_binary(&paths, &expected).await? {
                    return Ok(cached);
                }
                bail!(
                    "cache directory {} was created, but validation still failed",
                    paths.cache_dir.display()
                );
            }

            cleanup_dir(&staging_dir).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::installer::cache::BinaryCacheMetadata;
    use tempfile::tempdir;

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
        );
        let cached = BinaryCacheMetadata::new(
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            "https://example.com/other.tar.gz",
            "./bin/demo",
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
}
