//! Facilities for decoding the shared agent registry that `acp-agent` consumes.
//!
//! This module exposes the registry schema, platform selectors, and helpers to
//! search, validate, and fetch the remote JSON catalog that powers the
//! `acp-agent` CLI.

use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// URL for the canonical agent registry payload consumed by the CLI.
pub const REGISTRY_URL: &str =
    "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

/// CLI arguments forwarded to an agent's executable or package entry point.
pub type CommandArgs = Vec<String>;

/// Environment overrides applied before starting an agent.
pub type Environment = BTreeMap<String, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
/// Supported runtime targets for agent binaries.
///
/// Every agent catalog entry may include platform-specific binaries. The enum
/// mirrors the coordinator's operating system + architecture matrix so the
/// CLI can pick the fitting binary for the host that runs it.
pub enum Platform {
    /// macOS on Apple Silicon (`aarch64`).
    #[serde(rename = "darwin-aarch64")]
    DarwinAarch64,
    /// macOS on Intel (`x86_64`).
    #[serde(rename = "darwin-x86_64")]
    DarwinX86_64,
    /// Linux on arm64.
    #[serde(rename = "linux-aarch64")]
    LinuxAarch64,
    /// Linux on x86_64.
    #[serde(rename = "linux-x86_64")]
    LinuxX86_64,
    /// Windows on arm64.
    #[serde(rename = "windows-aarch64")]
    WindowsAarch64,
    /// Windows on x86_64.
    #[serde(rename = "windows-x86_64")]
    WindowsX86_64,
}

/// A single downloadable binary distribution for a particular platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryTarget {
    /// Remote archive that contains the binary package.
    pub archive: String,
    /// Relative path within the archive to the command that should be executed.
    pub cmd: String,
    /// Optional default command-line arguments that accompany the executable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<CommandArgs>,
    /// Optional environment variables that will be injected before execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<Environment>,
}

/// Binary references keyed by platform so the CLI can resolve the right file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryDistribution {
    #[serde(
        rename = "darwin-aarch64",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    /// Binary target published for macOS on Apple Silicon.
    pub darwin_aarch64: Option<BinaryTarget>,
    #[serde(
        rename = "darwin-x86_64",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    /// Binary target published for macOS on Intel.
    pub darwin_x86_64: Option<BinaryTarget>,
    #[serde(
        rename = "linux-aarch64",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    /// Binary target published for Linux on arm64.
    pub linux_aarch64: Option<BinaryTarget>,
    #[serde(
        rename = "linux-x86_64",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    /// Binary target published for Linux on x86_64.
    pub linux_x86_64: Option<BinaryTarget>,
    #[serde(
        rename = "windows-aarch64",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    /// Binary target published for Windows on arm64.
    pub windows_aarch64: Option<BinaryTarget>,
    #[serde(
        rename = "windows-x86_64",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    /// Binary target published for Windows on x86_64.
    pub windows_x86_64: Option<BinaryTarget>,
}

/// Metadata for package-based distributions (npm/uvx).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageDistribution {
    /// Identifier that the package manager understands.
    pub package: String,
    /// Default arguments appended to the package manager invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<CommandArgs>,
    /// Environment overrides that should apply when invoking the package manager.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<Environment>,
}

/// Alias for npm-based package distributions.
pub type NpxDistribution = PackageDistribution;
/// Alias for `uvx`-based package distributions.
pub type UvxDistribution = PackageDistribution;

/// Distribution channels that may be published for an agent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDistribution {
    /// Platform-specific binaries that can be downloaded and executed directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<BinaryDistribution>,
    /// `npx` package metadata when the agent ships as an npm package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npx: Option<NpxDistribution>,
    /// `uvx` package metadata for `uv` installed agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uvx: Option<UvxDistribution>,
}

impl AgentDistribution {
    /// Returns `true` if the agent references at least one install/run source.
    pub fn has_distribution_source(&self) -> bool {
        self.binary.is_some() || self.npx.is_some() || self.uvx.is_some()
    }

    fn validate(&self, path: &str) -> Result<()> {
        if self.has_distribution_source() {
            Ok(())
        } else {
            Err(registry_decode_error(format!(
                "{path}.distribution must contain at least one of binary, npx, or uvx"
            )))
        }
    }
}

impl BinaryDistribution {
    /// Returns the binary target registered for the given platform, if any.
    pub fn for_platform(&self, platform: Platform) -> Option<&BinaryTarget> {
        match platform {
            Platform::DarwinAarch64 => self.darwin_aarch64.as_ref(),
            Platform::DarwinX86_64 => self.darwin_x86_64.as_ref(),
            Platform::LinuxAarch64 => self.linux_aarch64.as_ref(),
            Platform::LinuxX86_64 => self.linux_x86_64.as_ref(),
            Platform::WindowsAarch64 => self.windows_aarch64.as_ref(),
            Platform::WindowsX86_64 => self.windows_x86_64.as_ref(),
        }
    }
}

/// Published metadata for a single ACP agent entry in the registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryAgent {
    /// Unique identifier for the published agent.
    pub id: String,
    /// Human-readable name for lists (`list` command sorting uses this).
    pub name: String,
    /// Semantic version string describing the published agent release.
    pub version: String,
    /// Short summary that surfaces in search and list output.
    pub description: String,
    /// Repository URL that correlates with the source code or project page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Optional marketing or documentation website for the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    /// Author credits declared by the agent publisher.
    pub authors: Vec<String>,
    /// SPDX or free-form license declaration.
    pub license: String,
    /// Optional emoji or image URL used as an icon when rendering the CLI list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Distribution metadata to determine how the agent is installed/run.
    pub distribution: AgentDistribution,
}

/// Top-level registry payload fetched from [`REGISTRY_URL`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Registry {
    /// Version label published alongside the registry payload.
    pub version: String,
    /// List of every registered agent exposed by the catalog.
    pub agents: Vec<RegistryAgent>,
    /// Optional extension data the catalog producer may append.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<Value>>,
}

impl Registry {
    /// Decodes the registry payload from a byte slice and validates it.
    pub fn from_slice(input: &[u8]) -> Result<Self> {
        let registry: Self = serde_json::from_slice(input).map_err(registry_decode_error)?;
        registry.validate()?;
        Ok(registry)
    }

    /// Decodes the registry from a `serde_json::Value` and validates it.
    pub fn from_value(input: Value) -> Result<Self> {
        let registry: Self = serde_json::from_value(input).map_err(registry_decode_error)?;
        registry.validate()?;
        Ok(registry)
    }

    /// Ensures every agent has at least one distribution source.
    pub fn validate(&self) -> Result<()> {
        for (index, agent) in self.agents.iter().enumerate() {
            let path = format!("agents[{index}]");
            agent.distribution.validate(&path)?;
        }

        Ok(())
    }

    /// Returns the raw `RegistryAgent` list that backs CLI commands/queries.
    pub fn list_agents(&self) -> &[RegistryAgent] {
        &self.agents
    }

    /// Finds an agent by `id`, returning `None` if no match exists.
    pub fn find_agent(&self, agent_id: &str) -> Option<&RegistryAgent> {
        self.agents.iter().find(|agent| agent.id == agent_id)
    }

    /// Retrieves an agent, failing if the `agent_id` is unknown.
    pub fn get_agent(&self, agent_id: &str) -> Result<&RegistryAgent> {
        self.find_agent(agent_id)
            .ok_or_else(|| anyhow!("agent with id \"{agent_id}\" was not found"))
    }

    /// Case-insensitive search across `id`, `name`, and `description`.
    ///
    /// An empty query returns all agents, just like the `list` command.
    pub fn search_agents(&self, query: &str) -> Vec<&RegistryAgent> {
        let needle = query.trim().to_ascii_lowercase();
        if needle.is_empty() {
            return self.agents.iter().collect();
        }

        self.agents
            .iter()
            .filter(|agent| {
                [
                    agent.id.as_str(),
                    agent.name.as_str(),
                    agent.description.as_str(),
                ]
                .into_iter()
                .any(|value| value.to_ascii_lowercase().contains(&needle))
            })
            .collect()
    }
}

impl FromStr for Registry {
    type Err = anyhow::Error;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let registry: Self = serde_json::from_str(input).map_err(registry_decode_error)?;
        registry.validate()?;
        Ok(registry)
    }
}

impl Platform {
    /// Detects the platform of the running process using `std::env::consts`.
    /// Returns an error when the OS/ARCH combination is not listed in the enum.
    pub fn current() -> Result<Self> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64") => Ok(Self::DarwinAarch64),
            ("macos", "x86_64") => Ok(Self::DarwinX86_64),
            ("linux", "aarch64") => Ok(Self::LinuxAarch64),
            ("linux", "x86_64") => Ok(Self::LinuxX86_64),
            ("windows", "aarch64") => Ok(Self::WindowsAarch64),
            ("windows", "x86_64") => Ok(Self::WindowsX86_64),
            (os, arch) => Err(anyhow!("unsupported platform: {os}-{arch}")),
        }
    }
}

/// Downloads the registry JSON and resolves it into a `Registry`.
pub async fn fetch_registry() -> Result<Registry> {
    let response = reqwest::get(REGISTRY_URL)
        .await
        .map_err(|error| anyhow!("failed to fetch registry payload: {error}"))?;
    let response = response
        .error_for_status()
        .map_err(|error| anyhow!("failed to fetch registry payload: {error}"))?;
    let bytes = response
        .bytes()
        .await
        .map_err(|error| anyhow!("failed to fetch registry payload: {error}"))?;
    Registry::from_slice(bytes.as_ref())
}

/// Normalizes decode failure errors so the caller knows which URL failed.
fn registry_decode_error(reason: impl std::fmt::Display) -> anyhow::Error {
    anyhow!("failed to decode registry payload from {REGISTRY_URL}: {reason}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_registry_with_binary_distribution() {
        let registry = Registry::from_value(json!({
            "version": "1",
            "agents": [
                {
                    "id": "test-agent",
                    "name": "Test Agent",
                    "version": "0.1.0",
                    "description": "Example agent",
                    "authors": ["ACP"],
                    "license": "MIT",
                    "distribution": {
                        "binary": {
                            "linux-x86_64": {
                                "archive": "https://example.com/test-agent.tar.gz",
                                "cmd": "test-agent"
                            }
                        }
                    }
                }
            ]
        }))
        .expect("registry should decode");

        let agent = registry
            .get_agent("test-agent")
            .expect("agent should exist");
        assert!(agent.distribution.binary.is_some());
        assert!(registry.search_agents("example").len() == 1);
    }

    #[test]
    fn rejects_distribution_without_any_source() {
        let error = Registry::from_value(json!({
            "version": "1",
            "agents": [
                {
                    "id": "broken-agent",
                    "name": "Broken Agent",
                    "version": "0.1.0",
                    "description": "Missing distribution payload",
                    "authors": ["ACP"],
                    "license": "MIT",
                    "distribution": {}
                }
            ]
        }))
        .expect_err("registry should reject empty distribution");

        assert!(
            error
                .to_string()
                .contains("distribution must contain at least one of binary, npx, or uvx")
        );
    }

    #[test]
    fn finds_agents_case_insensitively() {
        let registry = Registry::from_value(json!({
            "version": "1",
            "agents": [
                {
                    "id": "alpha",
                    "name": "Alpha Agent",
                    "version": "0.1.0",
                    "description": "First result",
                    "authors": ["ACP"],
                    "license": "MIT",
                    "distribution": {
                        "npx": {
                            "package": "@acp/alpha"
                        }
                    }
                },
                {
                    "id": "beta",
                    "name": "Beta Agent",
                    "version": "0.1.0",
                    "description": "Second result",
                    "authors": ["ACP"],
                    "license": "MIT",
                    "distribution": {
                        "uvx": {
                            "package": "acp-beta"
                        }
                    }
                }
            ]
        }))
        .expect("registry should decode");

        let results = registry.search_agents("ALPHA");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "alpha");
    }

    #[test]
    fn selects_binary_target_for_platform() {
        let distribution = BinaryDistribution {
            linux_x86_64: Some(BinaryTarget {
                archive: "https://example.com/tool.tar.gz".to_string(),
                cmd: "./tool".to_string(),
                args: None,
                env: None,
            }),
            ..Default::default()
        };

        let target = distribution
            .for_platform(Platform::LinuxX86_64)
            .expect("target should exist");
        assert_eq!(target.cmd, "./tool");
    }
}
