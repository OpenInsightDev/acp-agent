use super::staging::PreparedStaging;
use super::validation::validate_cached_binary_blocking;
use super::*;
/// Publishes a prepared cache in one blocking transaction.
///
/// The blocking task owns the staging directory and both cache locks. Dropping
/// the awaiting install future therefore only detaches a transaction that will
/// finish or roll back before releasing its locks; cancellation cannot strand
/// a backup or race a detached filesystem operation.
pub(crate) async fn promote_prepared_cache(
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
pub(crate) async fn promote_staged_cache(
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
/// Atomic publication state machine used by the detached blocking publisher.
///
/// The transaction owns the staging path and publication lock. If a rename or
/// post-publish validation fails, `Drop` removes the new directory and restores
/// the previous cache before releasing the lock.
pub(crate) struct PromotionTransaction {
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
pub(crate) fn unique_backup_dir_name(cache_dir: &Path) -> String {
    let version = cache_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("cache");
    unique_work_dir_name(version, "backup")
}

pub(crate) fn unique_work_dir_name(component: &str, kind: &str) -> String {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!(".{}-{kind}-{pid}-{nanos}", safe_path_component(component))
}
