use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs;

use crate::registry::Platform;

const CACHE_NAMESPACE: &str = "acp-agent";
pub(crate) const AGENTS_DIR: &str = "agents";
pub(crate) const EXTRACTED_DIR_NAME: &str = "extracted";
pub(crate) const METADATA_FILE_NAME: &str = "metadata.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BinaryCachePaths {
    pub root_dir: PathBuf,
    pub parent_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub extracted_dir: PathBuf,
    pub metadata_path: PathBuf,
}

/// Returns the stable sibling lock path for one immutable cache key.
///
/// The lock lives beside (rather than inside) the final directory so it is
/// never renamed during publication or removed during cache cleanup.
pub(crate) fn binary_cache_lock_path(paths: &BinaryCachePaths) -> PathBuf {
    paths.parent_dir.join(format!(
        "{}-cache.lock",
        safe_path_component(
            paths
                .cache_dir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("cache")
        )
    ))
}

/// Returns the stable sibling lock path that protects payload use and removal.
/// Keeping it separate from the publication lock permits validated readers to
/// run concurrently while writers still serialize metadata transitions.
pub(crate) fn binary_cache_use_lock_path(paths: &BinaryCachePaths) -> PathBuf {
    paths.parent_dir.join(format!(
        "{}-cache.use.lock",
        safe_path_component(
            paths
                .cache_dir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("cache")
        )
    ))
}

/// Acquires an exclusive advisory lock for one cache key.
///
/// `fd-lock` may block while another process publishes or removes the entry,
/// so the blocking operation is isolated on a Tokio blocking worker.
pub(crate) async fn acquire_binary_cache_lock(paths: &BinaryCachePaths) -> Result<BinaryCacheLock> {
    let lock_path = binary_cache_lock_path(paths);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std_mpsc::channel::<()>();
    tokio::task::spawn_blocking(move || {
        let file = match std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(file) => file,
            Err(error) => {
                let _ = ready_tx.send(Err(error.to_string()));
                return;
            }
        };
        let mut lock = fd_lock::RwLock::new(file);
        let guard = match lock.write() {
            Ok(guard) => guard,
            Err(error) => {
                let _ = ready_tx.send(Err(error.to_string()));
                return;
            }
        };
        if ready_tx.send(Ok(())).is_err() {
            return;
        }
        let _ = release_rx.recv();
        drop(guard);
    });
    match ready_rx.await {
        Ok(Ok(())) => Ok(BinaryCacheLock {
            release: Some(release_tx),
        }),
        Ok(Err(error)) => Err(anyhow!("failed to acquire cache lock: {error}")),
        Err(_) => Err(anyhow!("cache lock task terminated unexpectedly")),
    }
}

/// Acquires a shared cache-key lease for a process that will execute the
/// validated payload. Writers and removers wait until every active reader has
/// released this lease.
pub(crate) async fn acquire_binary_cache_use_read_lock(
    paths: &BinaryCachePaths,
) -> Result<BinaryCacheLock> {
    let lock_path = binary_cache_use_lock_path(paths);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std_mpsc::channel::<()>();
    tokio::task::spawn_blocking(move || {
        let file = match std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(file) => file,
            Err(error) => {
                let _ = ready_tx.send(Err(error.to_string()));
                return;
            }
        };
        let lock = fd_lock::RwLock::new(file);
        let guard = match lock.read() {
            Ok(guard) => guard,
            Err(error) => {
                let _ = ready_tx.send(Err(error.to_string()));
                return;
            }
        };
        if ready_tx.send(Ok(())).is_err() {
            return;
        }
        let _ = release_rx.recv();
        drop(guard);
    });
    match ready_rx.await {
        Ok(Ok(())) => Ok(BinaryCacheLock {
            release: Some(release_tx),
        }),
        Ok(Err(error)) => Err(anyhow!("failed to acquire cache use lock: {error}")),
        Err(_) => Err(anyhow!("cache use-lock task terminated unexpectedly")),
    }
}

/// Acquires the exclusive payload-use lock used before replacing or deleting
/// a cache directory.
pub(crate) async fn acquire_binary_cache_use_write_lock(
    paths: &BinaryCachePaths,
) -> Result<BinaryCacheLock> {
    let lock_path = binary_cache_use_lock_path(paths);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std_mpsc::channel::<()>();
    tokio::task::spawn_blocking(move || {
        let file = match std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(file) => file,
            Err(error) => {
                let _ = ready_tx.send(Err(error.to_string()));
                return;
            }
        };
        let mut lock = fd_lock::RwLock::new(file);
        let guard = match lock.write() {
            Ok(guard) => guard,
            Err(error) => {
                let _ = ready_tx.send(Err(error.to_string()));
                return;
            }
        };
        if ready_tx.send(Ok(())).is_err() {
            return;
        }
        let _ = release_rx.recv();
        drop(guard);
    });
    match ready_rx.await {
        Ok(Ok(())) => Ok(BinaryCacheLock {
            release: Some(release_tx),
        }),
        Ok(Err(error)) => Err(anyhow!("failed to acquire cache use lock: {error}")),
        Err(_) => Err(anyhow!("cache use-lock task terminated unexpectedly")),
    }
}

/// Attempts to acquire a cache key without waiting for another process.
/// Startup cleanup uses this to skip active installs rather than delaying
/// every CLI invocation behind a long download or extraction.
pub(crate) async fn try_acquire_binary_cache_lock(
    paths: &BinaryCachePaths,
) -> Result<Option<BinaryCacheLock>> {
    let lock_path = binary_cache_lock_path(paths);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std_mpsc::channel::<()>();
    tokio::task::spawn_blocking(move || {
        let file = match std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(file) => file,
            Err(error) => {
                let _ = ready_tx.send(Err(error.to_string()));
                return;
            }
        };
        let mut lock = fd_lock::RwLock::new(file);
        let guard = match lock.try_write() {
            Ok(guard) => guard,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let _ = ready_tx.send(Ok(false));
                return;
            }
            Err(error) => {
                let _ = ready_tx.send(Err(error.to_string()));
                return;
            }
        };
        if ready_tx.send(Ok(true)).is_err() {
            return;
        }
        let _ = release_rx.recv();
        drop(guard);
    });
    match ready_rx.await {
        Ok(Ok(true)) => Ok(Some(BinaryCacheLock {
            release: Some(release_tx),
        })),
        Ok(Ok(false)) => Ok(None),
        Ok(Err(error)) => Err(anyhow!("failed to acquire cache lock: {error}")),
        Err(_) => Err(anyhow!("cache lock task terminated unexpectedly")),
    }
}

pub(crate) struct BinaryCacheLock {
    release: Option<std_mpsc::Sender<()>>,
}

impl fmt::Debug for BinaryCacheLock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BinaryCacheLock")
    }
}

impl Drop for BinaryCacheLock {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BinaryCacheMetadata {
    pub agent_id: String,
    pub agent_version: String,
    pub platform: String,
    pub archive: String,
    pub cmd: String,
    /// Optional SHA-256 of the downloaded archive. Binding the digest into the
    /// metadata means a re-published archive (or a newly published digest)
    /// invalidates the cached entry through the existing equality check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// SHA-256 of the extracted executable. Used to detect post-install
    /// modification of a cache entry before it is executed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable_sha256: Option<String>,
    /// Deterministic SHA-256 of the complete extracted payload tree. This
    /// catches modifications to libraries and auxiliary files beside the
    /// executable before a cache entry is reused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_sha256: Option<String>,
}

impl BinaryCacheMetadata {
    pub(crate) fn new(
        agent_id: &str,
        agent_version: &str,
        platform: Platform,
        archive: &str,
        cmd: &str,
        sha256: Option<&str>,
    ) -> Self {
        Self {
            agent_id: agent_id.to_string(),
            agent_version: agent_version.to_string(),
            platform: platform_cache_key(platform).to_string(),
            archive: archive.to_string(),
            cmd: cmd.to_string(),
            sha256: sha256.map(|digest| digest.to_ascii_lowercase()),
            executable_sha256: None,
            payload_sha256: None,
        }
    }
}

pub(crate) fn cache_root_dir() -> Result<PathBuf> {
    let root = dirs::cache_dir()
        .ok_or_else(|| anyhow!("could not determine the platform cache directory"))?;
    Ok(root.join(CACHE_NAMESPACE))
}

pub(crate) fn binary_cache_paths(
    root_dir: &Path,
    agent_id: &str,
    agent_version: &str,
    platform: Platform,
) -> BinaryCachePaths {
    let parent_dir = root_dir
        .join(AGENTS_DIR)
        .join(safe_path_component(agent_id))
        .join(platform_cache_key(platform));
    let cache_dir = parent_dir.join(safe_path_component(agent_version));

    BinaryCachePaths {
        root_dir: root_dir.to_path_buf(),
        parent_dir,
        extracted_dir: cache_dir.join(EXTRACTED_DIR_NAME),
        metadata_path: cache_dir.join(METADATA_FILE_NAME),
        cache_dir,
    }
}

/// Returns cache paths keyed by the immutable registry target digest.
/// Callers validate the digest before passing it here; digest-less registry
/// targets are rejected rather than mapped into the historical mutable key.
pub(crate) fn binary_cache_paths_with_digest(
    root_dir: &Path,
    agent_id: &str,
    agent_version: &str,
    platform: Platform,
    sha256: &str,
) -> BinaryCachePaths {
    let key = format!("{}-sha256-{}", agent_version, sha256.to_ascii_lowercase());
    binary_cache_paths(root_dir, agent_id, &key, platform)
}

pub(crate) fn platform_cache_key(platform: Platform) -> &'static str {
    match platform {
        Platform::DarwinAarch64 => "darwin-aarch64",
        Platform::DarwinX86_64 => "darwin-x86_64",
        Platform::LinuxAarch64 => "linux-aarch64",
        Platform::LinuxX86_64 => "linux-x86_64",
        Platform::WindowsAarch64 => "windows-aarch64",
        Platform::WindowsX86_64 => "windows-x86_64",
    }
}

/// Builds a filesystem-safe path component for a registry ID or version.
///
/// Values that are already safe (`demo`, `1.2.3`, `a_b`) pass through
/// unchanged so existing cache paths stay stable. Whenever sanitization is
/// lossy (`a/b`, `a:b`, dot-only values), a stable digest of the original
/// value is appended so distinct logical IDs can never resolve to the same
/// directory. The digest is derived from the original value, not from the
/// lossy form, so `a/b` and `a_b` remain distinct even though both sanitize
/// to the same prefix.
pub(crate) fn safe_path_component(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    let mut lossy = value.is_empty();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            sanitized.push(ch);
        } else {
            lossy = true;
            sanitized.push('_');
        }
    }

    let trimmed = match sanitized.trim_matches('.') {
        "" => "_".to_string(),
        trimmed => trimmed.to_string(),
    };

    if lossy || trimmed != value {
        format!("{trimmed}-{}", stable_path_digest(value))
    } else {
        trimmed
    }
}

/// Short stable digest used to disambiguate lossy path components.
fn stable_path_digest(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut hex = String::with_capacity(12);
    for byte in digest.iter().take(6) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// A binary distribution discovered in the local agent cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CachedAgent {
    /// Registry ID of the cached agent.
    #[serde(rename = "id")]
    pub agent_id: String,
    /// Agent version held by this cache entry.
    #[serde(rename = "version")]
    pub agent_version: String,
    /// Platform cache key (for example `linux-x86_64`) of this entry.
    pub platform: String,
    /// Stable cache directory that owns the extracted payload.
    pub cache_dir: PathBuf,
    /// Executable entry point inside the extracted payload.
    pub executable_path: PathBuf,
}

/// Scans the cache for every installed binary distribution.
///
/// Entries with missing or corrupt metadata are skipped so a damaged cache
/// never breaks `list --installed`; staging directories left behind by
/// interrupted installs are ignored as well.
pub(crate) async fn list_cached_agents(root_dir: &Path) -> Vec<CachedAgent> {
    let mut agents = Vec::new();
    let agents_dir = root_dir.join(AGENTS_DIR);
    let Ok(mut agent_entries) = fs::read_dir(&agents_dir).await else {
        return agents;
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
                if version_entry.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                if let Some(agent) = read_cached_agent(&version_entry.path()).await {
                    agents.push(agent);
                }
            }
        }
    }

    agents
}

async fn read_cached_agent(cache_dir: &Path) -> Option<CachedAgent> {
    let metadata_bytes = fs::read(cache_dir.join(METADATA_FILE_NAME)).await.ok()?;
    let metadata: BinaryCacheMetadata = serde_json::from_slice(&metadata_bytes).ok()?;
    Some(CachedAgent {
        agent_id: metadata.agent_id,
        agent_version: metadata.agent_version,
        platform: metadata.platform,
        cache_dir: cache_dir.to_path_buf(),
        executable_path: cache_dir.join(EXTRACTED_DIR_NAME).join(&metadata.cmd),
    })
}

/// Removes every cached binary distribution for an agent.
///
/// Returns `true` when at least one cache entry was removed.
pub(crate) async fn remove_cached_agent(root_dir: &Path, agent_id: &str) -> Result<bool> {
    remove_cached_entries(root_dir, agent_id, None, None).await
}

/// Removes all matching entries except the cache directory that was just
/// installed. Metadata identity is authoritative because sanitized path
/// components are not collision-free.
#[cfg(test)]
pub(crate) async fn remove_cached_platform_except(
    root_dir: &Path,
    agent_id: &str,
    platform: Platform,
    keep: &Path,
) -> Result<bool> {
    remove_cached_entries(
        root_dir,
        agent_id,
        Some(platform_cache_key(platform)),
        Some(keep),
    )
    .await
}

async fn remove_cached_entries(
    root_dir: &Path,
    agent_id: &str,
    platform: Option<&str>,
    keep: Option<&Path>,
) -> Result<bool> {
    let mut entries = list_cached_agents(root_dir).await;
    // Inventory intentionally ignores corrupt metadata, but uninstall must
    // still be able to remove a damaged entry. The agent namespace is derived
    // from the same collision-resistant component used by cache publication,
    // so scanning it cannot select another logical agent.
    let known_paths = entries
        .iter()
        .map(|entry| entry.cache_dir.clone())
        .collect::<std::collections::HashSet<_>>();
    let agent_dir = root_dir
        .join(AGENTS_DIR)
        .join(safe_path_component(agent_id));
    if let Ok(mut platform_entries) = fs::read_dir(&agent_dir).await {
        while let Ok(Some(platform_entry)) = platform_entries.next_entry().await {
            let platform_name = platform_entry.file_name().to_string_lossy().into_owned();
            if platform.is_some_and(|expected| expected != platform_name) {
                continue;
            }
            let Ok(file_type) = platform_entry.file_type().await else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let Ok(mut version_entries) = fs::read_dir(platform_entry.path()).await else {
                continue;
            };
            while let Ok(Some(version_entry)) = version_entries.next_entry().await {
                let name = version_entry.file_name();
                if name.to_string_lossy().starts_with('.') {
                    continue;
                }
                let Ok(file_type) = version_entry.file_type().await else {
                    continue;
                };
                if !file_type.is_dir() || known_paths.contains(&version_entry.path()) {
                    continue;
                }
                entries.push(CachedAgent {
                    agent_id: agent_id.to_string(),
                    agent_version: name.to_string_lossy().into_owned(),
                    platform: platform_name.clone(),
                    cache_dir: version_entry.path(),
                    executable_path: PathBuf::new(),
                });
            }
        }
    }
    let mut removed = false;

    for entry in entries {
        if entry.agent_id != agent_id
            || platform.is_some_and(|platform| entry.platform != platform)
            || keep.is_some_and(|keep| entry.cache_dir == keep)
        {
            continue;
        }

        // Hold the same per-key lock used by installers while deleting the
        // final directory. This prevents uninstall/update cleanup from
        // deleting a cache another process has just validated or published.
        let paths = BinaryCachePaths {
            root_dir: root_dir.to_path_buf(),
            parent_dir: entry
                .cache_dir
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| root_dir.to_path_buf()),
            extracted_dir: entry.cache_dir.join(EXTRACTED_DIR_NAME),
            metadata_path: entry.cache_dir.join(METADATA_FILE_NAME),
            cache_dir: entry.cache_dir.clone(),
        };
        let _lock = acquire_binary_cache_lock(&paths).await?;
        let _use_lock = acquire_binary_cache_use_write_lock(&paths).await?;
        match fs::remove_dir_all(&entry.cache_dir).await {
            Ok(()) => {
                removed = true;
                remove_empty_cache_parents(root_dir, &entry.cache_dir).await;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to remove cache directory {}",
                        entry.cache_dir.display()
                    )
                });
            }
        }
    }

    Ok(removed)
}

async fn remove_empty_cache_parents(root_dir: &Path, cache_dir: &Path) {
    let agents_dir = root_dir.join(AGENTS_DIR);
    let Some(platform_dir) = cache_dir.parent() else {
        return;
    };
    let Some(agent_dir) = platform_dir.parent() else {
        return;
    };

    let _ = fs::remove_dir(platform_dir).await;
    let _ = fs::remove_dir(agent_dir).await;
    let _ = fs::remove_dir(agents_dir).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn builds_binary_cache_paths_under_namespace() {
        let root_dir = Path::new("/tmp/cache").join("acp-agent");
        let paths = binary_cache_paths(
            root_dir.as_path(),
            "demo/agent",
            "1.2.3",
            Platform::LinuxX86_64,
        );

        assert_eq!(paths.root_dir, root_dir);
        assert_eq!(
            paths.parent_dir,
            Path::new("/tmp/cache")
                .join("acp-agent")
                .join("agents")
                .join("demo_agent-0b26afc5211b")
                .join("linux-x86_64")
        );
        assert_eq!(
            paths.cache_dir,
            Path::new("/tmp/cache")
                .join("acp-agent")
                .join("agents")
                .join("demo_agent-0b26afc5211b")
                .join("linux-x86_64")
                .join("1.2.3")
        );
    }

    #[test]
    fn keeps_clean_path_components_unchanged() {
        assert_eq!(safe_path_component("demo"), "demo");
        assert_eq!(safe_path_component("1.2.3"), "1.2.3");
        assert_eq!(safe_path_component("demo_agent"), "demo_agent");
        assert_eq!(safe_path_component("a.b-c_d"), "a.b-c_d");
    }

    #[test]
    fn disambiguates_lossy_path_components_with_a_stable_digest() {
        assert_eq!(
            safe_path_component("demo/agent:beta"),
            "demo_agent_beta-c1a81a1eef70"
        );
        assert_eq!(safe_path_component("..."), "_-ab5df625bc76");
        assert_eq!(safe_path_component("."), "_-cdb4ee2aea69");
        // The digest is deterministic for the same input.
        assert_eq!(safe_path_component("demo/agent"), "demo_agent-0b26afc5211b");
        assert_eq!(safe_path_component("demo/agent"), "demo_agent-0b26afc5211b");
    }

    #[test]
    fn digest_bound_cache_paths_are_case_insensitive() {
        let root = Path::new("/tmp/cache");
        let lower = binary_cache_paths_with_digest(
            root,
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            &"ab".repeat(32),
        );
        let upper = binary_cache_paths_with_digest(
            root,
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            &"AB".repeat(32),
        );
        assert_eq!(lower.cache_dir, upper.cache_dir);
    }

    #[test]
    fn distinct_logical_ids_never_resolve_to_the_same_directory() {
        let mut components = [
            safe_path_component("a/b"),
            safe_path_component("a:b"),
            safe_path_component("a?b"),
            safe_path_component("a_b"),
            safe_path_component("."),
            safe_path_component("_"),
            safe_path_component(""),
        ]
        .to_vec();
        components.sort_unstable();
        components.dedup();

        assert_eq!(
            components.len(),
            7,
            "distinct logical IDs and versions must map to distinct directories"
        );
    }

    #[tokio::test]
    async fn lists_cached_binaries_across_agents_and_platforms() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let demo_100 = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        let demo_110 = binary_cache_paths(&cache_root, "demo", "1.1.0", Platform::LinuxX86_64);
        let zebra = binary_cache_paths(&cache_root, "zebra", "0.2.0", Platform::DarwinAarch64);

        for (paths, agent_id, version, platform, cmd) in [
            (
                &demo_100,
                "demo",
                "1.0.0",
                Platform::LinuxX86_64,
                "./bin/demo",
            ),
            (
                &demo_110,
                "demo",
                "1.1.0",
                Platform::LinuxX86_64,
                "./bin/demo",
            ),
            (&zebra, "zebra", "0.2.0", Platform::DarwinAarch64, "zebra"),
        ] {
            fs::create_dir_all(&paths.cache_dir).await.unwrap();
            let metadata = BinaryCacheMetadata::new(
                agent_id,
                version,
                platform,
                "https://example.com/agent.tar.gz",
                cmd,
                None,
            );
            fs::write(&paths.metadata_path, serde_json::to_vec(&metadata).unwrap())
                .await
                .unwrap();
        }

        let cached = list_cached_agents(&cache_root).await;
        assert_eq!(cached.len(), 3);

        let demo_100_entry = cached
            .iter()
            .find(|agent| agent.agent_version == "1.0.0")
            .unwrap();
        assert_eq!(demo_100_entry.agent_id, "demo");
        assert_eq!(demo_100_entry.platform, "linux-x86_64");
        assert_eq!(demo_100_entry.cache_dir, demo_100.cache_dir);
        assert_eq!(
            demo_100_entry.executable_path,
            demo_100
                .cache_dir
                .join("extracted")
                .join("bin")
                .join("demo")
        );

        let zebra_entry = cached
            .iter()
            .find(|agent| agent.agent_id == "zebra")
            .unwrap();
        assert_eq!(zebra_entry.agent_version, "0.2.0");
        assert_eq!(zebra_entry.platform, "darwin-aarch64");
    }

    #[tokio::test]
    async fn skips_corrupt_and_staging_entries_when_listing() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        fs::create_dir_all(&paths.cache_dir).await.unwrap();
        fs::write(&paths.metadata_path, b"{not-json").await.unwrap();

        let staging_dir = paths.parent_dir.join(".1.0.0-staging-1-2");
        fs::create_dir_all(&staging_dir).await.unwrap();
        let metadata = BinaryCacheMetadata::new(
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            "https://example.com/a",
            "./demo",
            None,
        );
        fs::write(
            staging_dir.join(METADATA_FILE_NAME),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .await
        .unwrap();

        assert!(list_cached_agents(&cache_root).await.is_empty());
    }

    #[tokio::test]
    async fn removes_cached_agent_directories() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        write_cache_entry(&paths, "demo", "1.0.0", Platform::LinuxX86_64).await;

        assert!(remove_cached_agent(&cache_root, "demo").await.unwrap());
        assert!(!remove_cached_agent(&cache_root, "demo").await.unwrap());
        assert!(!remove_cached_agent(&cache_root, "absent").await.unwrap());
    }

    #[tokio::test]
    async fn uninstall_removes_corrupt_cache_without_metadata() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths_with_digest(
            &cache_root,
            "demo",
            "1.0.0",
            Platform::LinuxX86_64,
            &"a".repeat(64),
        );
        fs::create_dir_all(&paths.extracted_dir).await.unwrap();
        fs::write(&paths.metadata_path, b"not-json").await.unwrap();

        assert!(remove_cached_agent(&cache_root, "demo").await.unwrap());
        assert!(!paths.cache_dir.exists());
    }

    #[tokio::test]
    async fn removes_only_requested_platform_when_updating() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let linux = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        let darwin = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::DarwinAarch64);
        write_cache_entry(&linux, "demo", "1.0.0", Platform::LinuxX86_64).await;
        write_cache_entry(&darwin, "demo", "1.0.0", Platform::DarwinAarch64).await;

        assert!(
            remove_cached_platform_except(
                &cache_root,
                "demo",
                Platform::LinuxX86_64,
                &cache_root.join("does-not-exist"),
            )
            .await
            .unwrap()
        );
        assert!(darwin.cache_dir.exists());
        assert!(!linux.cache_dir.exists());
    }

    #[tokio::test]
    async fn removal_is_keyed_by_exact_metadata_identity_not_path() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let underscore = binary_cache_paths(&cache_root, "_", "1.0.0", Platform::LinuxX86_64);
        let dot = binary_cache_paths(&cache_root, ".", "1.0.0", Platform::LinuxX86_64);
        write_cache_entry(&underscore, "_", "1.0.0", Platform::LinuxX86_64).await;
        write_cache_entry(&dot, ".", "1.0.0", Platform::LinuxX86_64).await;

        assert!(!remove_cached_agent(&cache_root, "absent").await.unwrap());
        assert!(underscore.cache_dir.exists());
        assert!(dot.cache_dir.exists());
        assert!(remove_cached_agent(&cache_root, ".").await.unwrap());
        assert!(!dot.cache_dir.exists());
        assert!(underscore.cache_dir.exists());
    }

    #[tokio::test]
    async fn platform_cleanup_preserves_the_newly_installed_version() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let old = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        let current = binary_cache_paths(&cache_root, "demo", "2.0.0", Platform::LinuxX86_64);
        write_cache_entry(&old, "demo", "1.0.0", Platform::LinuxX86_64).await;
        write_cache_entry(&current, "demo", "2.0.0", Platform::LinuxX86_64).await;

        assert!(
            remove_cached_platform_except(
                &cache_root,
                "demo",
                Platform::LinuxX86_64,
                &current.cache_dir,
            )
            .await
            .unwrap()
        );
        assert!(!old.cache_dir.exists());
        assert!(current.cache_dir.exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn removal_waits_for_the_cache_key_lock() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        write_cache_entry(&paths, "demo", "1.0.0", Platform::LinuxX86_64).await;

        let lock = acquire_binary_cache_lock(&paths).await.unwrap();
        let task_root = cache_root.clone();
        let removal = tokio::spawn(async move { remove_cached_agent(&task_root, "demo").await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(paths.cache_dir.exists());
        assert!(
            !removal.is_finished(),
            "removal should wait for the publisher lock"
        );

        drop(lock);
        assert!(removal.await.unwrap().unwrap());
        assert!(!paths.cache_dir.exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn removal_waits_for_active_payload_use_lease() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let paths = binary_cache_paths(&cache_root, "demo", "1.0.0", Platform::LinuxX86_64);
        write_cache_entry(&paths, "demo", "1.0.0", Platform::LinuxX86_64).await;

        let lease = acquire_binary_cache_use_read_lock(&paths).await.unwrap();
        let task_root = cache_root.clone();
        let removal = tokio::spawn(async move { remove_cached_agent(&task_root, "demo").await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(paths.cache_dir.exists());
        assert!(
            !removal.is_finished(),
            "removal should wait while a runner owns the payload lease"
        );

        drop(lease);
        assert!(removal.await.unwrap().unwrap());
        assert!(!paths.cache_dir.exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_a_lock_waiter_does_not_leak_the_lock() {
        let temp_dir = tempdir().unwrap();
        let paths = binary_cache_paths(temp_dir.path(), "demo", "1.0.0", Platform::LinuxX86_64);
        fs::create_dir_all(&paths.parent_dir).await.unwrap();
        let held = acquire_binary_cache_lock(&paths).await.unwrap();
        let waiter_paths = paths.clone();
        let waiter = tokio::spawn(async move { acquire_binary_cache_lock(&waiter_paths).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        waiter.abort();
        assert!(waiter.await.is_err());

        drop(held);
        let reacquired = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            acquire_binary_cache_lock(&paths),
        )
        .await
        .expect("cancelled waiter must release after acquiring")
        .unwrap();
        drop(reacquired);
    }

    async fn write_cache_entry(
        paths: &BinaryCachePaths,
        agent_id: &str,
        version: &str,
        platform: Platform,
    ) {
        fs::create_dir_all(&paths.cache_dir).await.unwrap();
        let metadata = BinaryCacheMetadata::new(
            agent_id,
            version,
            platform,
            "https://example.com/agent.tar.gz",
            "./agent",
            None,
        );
        fs::write(&paths.metadata_path, serde_json::to_vec(&metadata).unwrap())
            .await
            .unwrap();
    }
}
