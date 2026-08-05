use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::registry::Platform;

const CACHE_NAMESPACE: &str = "acp-agent";
const AGENTS_DIR: &str = "agents";
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
            sha256: sha256.map(str::to_owned),
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

pub(crate) fn safe_path_component(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            sanitized.push(ch);
        } else {
            sanitized.push('_');
        }
    }

    match sanitized.trim_matches('.') {
        "" => "_".to_string(),
        trimmed => trimmed.to_string(),
    }
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
    let entries = list_cached_agents(root_dir).await;
    let mut removed = false;

    for entry in entries {
        if entry.agent_id != agent_id
            || platform.is_some_and(|platform| entry.platform != platform)
            || keep.is_some_and(|keep| entry.cache_dir == keep)
        {
            continue;
        }

        fs::remove_dir_all(&entry.cache_dir)
            .await
            .with_context(|| {
                format!(
                    "failed to remove cache directory {}",
                    entry.cache_dir.display()
                )
            })?;
        removed = true;
        remove_empty_cache_parents(root_dir, &entry.cache_dir).await;
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
                .join("demo_agent")
                .join("linux-x86_64")
        );
        assert_eq!(
            paths.cache_dir,
            Path::new("/tmp/cache")
                .join("acp-agent")
                .join("agents")
                .join("demo_agent")
                .join("linux-x86_64")
                .join("1.2.3")
        );
    }

    #[test]
    fn sanitizes_non_filename_characters() {
        assert_eq!(safe_path_component("demo/agent:beta"), "demo_agent_beta");
        assert_eq!(safe_path_component("..."), "_");
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
    async fn removal_uses_exact_metadata_identity_when_paths_collide() {
        let temp_dir = tempdir().unwrap();
        let cache_root = temp_dir.path().join("cache").join("acp-agent");
        let colliding = binary_cache_paths(&cache_root, "_", "1.0.0", Platform::LinuxX86_64);
        write_cache_entry(&colliding, "_", "1.0.0", Platform::LinuxX86_64).await;

        assert!(!remove_cached_agent(&cache_root, ".").await.unwrap());
        assert!(colliding.cache_dir.exists());
        assert!(remove_cached_agent(&cache_root, "_").await.unwrap());
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
