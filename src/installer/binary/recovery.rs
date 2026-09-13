use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use tokio::fs;

use crate::installer::cache::{
    AGENTS_DIR, BinaryCachePaths, EXTRACTED_DIR_NAME, METADATA_FILE_NAME, cache_root_dir,
    try_acquire_binary_cache_lock,
};
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
pub(crate) async fn clean_stale_staging_entries_in(root_dir: &Path) -> usize {
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

pub(crate) fn work_dir_cache_key(name: &str) -> Option<&str> {
    let name = name.strip_prefix('.')?;
    ["-staging-", "-backup-"]
        .into_iter()
        .filter_map(|marker| name.rfind(marker).map(|index| (index, &name[..index])))
        .max_by_key(|(index, _)| *index)
        .map(|(_, key)| key)
        .filter(|key| !key.is_empty())
}
