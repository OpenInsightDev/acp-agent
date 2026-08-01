use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

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
}

impl BinaryCacheMetadata {
    pub(crate) fn new(
        agent_id: &str,
        agent_version: &str,
        platform: Platform,
        archive: &str,
        cmd: &str,
    ) -> Self {
        Self {
            agent_id: agent_id.to_string(),
            agent_version: agent_version.to_string(),
            platform: platform_cache_key(platform).to_string(),
            archive: archive.to_string(),
            cmd: cmd.to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
