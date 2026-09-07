use super::*;
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

pub(crate) async fn cache_binary_target_in_mode(
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
