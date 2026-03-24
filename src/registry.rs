use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const REGISTRY_URL: &str =
    "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

pub type CommandArgs = Vec<String>;
pub type Environment = BTreeMap<String, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Platform {
    #[serde(rename = "darwin-aarch64")]
    DarwinAarch64,
    #[serde(rename = "darwin-x86_64")]
    DarwinX86_64,
    #[serde(rename = "linux-aarch64")]
    LinuxAarch64,
    #[serde(rename = "linux-x86_64")]
    LinuxX86_64,
    #[serde(rename = "windows-aarch64")]
    WindowsAarch64,
    #[serde(rename = "windows-x86_64")]
    WindowsX86_64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryTarget {
    pub archive: String,
    pub cmd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<CommandArgs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<Environment>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryDistribution {
    #[serde(
        rename = "darwin-aarch64",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub darwin_aarch64: Option<BinaryTarget>,
    #[serde(
        rename = "darwin-x86_64",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub darwin_x86_64: Option<BinaryTarget>,
    #[serde(
        rename = "linux-aarch64",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub linux_aarch64: Option<BinaryTarget>,
    #[serde(
        rename = "linux-x86_64",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub linux_x86_64: Option<BinaryTarget>,
    #[serde(
        rename = "windows-aarch64",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub windows_aarch64: Option<BinaryTarget>,
    #[serde(
        rename = "windows-x86_64",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub windows_x86_64: Option<BinaryTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageDistribution {
    pub package: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<CommandArgs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<Environment>,
}

pub type NpxDistribution = PackageDistribution;
pub type UvxDistribution = PackageDistribution;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDistribution {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<BinaryDistribution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npx: Option<NpxDistribution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uvx: Option<UvxDistribution>,
}

impl AgentDistribution {
    pub fn has_distribution_source(&self) -> bool {
        self.binary.is_some() || self.npx.is_some() || self.uvx.is_some()
    }

    fn validate(&self, path: &str) -> Result<(), RegistryDecodeError> {
        if self.has_distribution_source() {
            Ok(())
        } else {
            Err(RegistryDecodeError::new(
                REGISTRY_URL,
                format!("{path}.distribution must contain at least one of binary, npx, or uvx"),
            ))
        }
    }
}

impl BinaryDistribution {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryAgent {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    pub authors: Vec<String>,
    pub license: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub distribution: AgentDistribution,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Registry {
    pub version: String,
    pub agents: Vec<RegistryAgent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<Value>>,
}

impl Registry {
    pub fn from_str(input: &str) -> Result<Self, RegistryDecodeError> {
        let registry: Self = serde_json::from_str(input).map_err(RegistryDecodeError::from_json)?;
        registry.validate()?;
        Ok(registry)
    }

    pub fn from_slice(input: &[u8]) -> Result<Self, RegistryDecodeError> {
        let registry: Self =
            serde_json::from_slice(input).map_err(RegistryDecodeError::from_json)?;
        registry.validate()?;
        Ok(registry)
    }

    pub fn from_value(input: Value) -> Result<Self, RegistryDecodeError> {
        let registry: Self =
            serde_json::from_value(input).map_err(RegistryDecodeError::from_json)?;
        registry.validate()?;
        Ok(registry)
    }

    pub fn validate(&self) -> Result<(), RegistryDecodeError> {
        for (index, agent) in self.agents.iter().enumerate() {
            let path = format!("agents[{index}]");
            agent.distribution.validate(&path)?;
        }

        Ok(())
    }

    pub fn list_agents(&self) -> &[RegistryAgent] {
        &self.agents
    }

    pub fn find_agent(&self, agent_id: &str) -> Option<&RegistryAgent> {
        self.agents.iter().find(|agent| agent.id == agent_id)
    }

    pub fn get_agent(&self, agent_id: &str) -> Result<&RegistryAgent, AgentNotFoundError> {
        self.find_agent(agent_id)
            .ok_or_else(|| AgentNotFoundError::new(agent_id))
    }

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

impl Platform {
    pub fn current() -> Result<Self, UnsupportedPlatformError> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64") => Ok(Self::DarwinAarch64),
            ("macos", "x86_64") => Ok(Self::DarwinX86_64),
            ("linux", "aarch64") => Ok(Self::LinuxAarch64),
            ("linux", "x86_64") => Ok(Self::LinuxX86_64),
            ("windows", "aarch64") => Ok(Self::WindowsAarch64),
            ("windows", "x86_64") => Ok(Self::WindowsX86_64),
            (os, arch) => Err(UnsupportedPlatformError::new(os, arch)),
        }
    }
}

pub async fn fetch_registry() -> Result<Registry, FetchRegistryError> {
    let response = reqwest::get(REGISTRY_URL)
        .await
        .map_err(FetchRegistryError::Request)?;
    let response = response
        .error_for_status()
        .map_err(FetchRegistryError::Request)?;
    let bytes = response
        .bytes()
        .await
        .map_err(FetchRegistryError::Request)?;
    Registry::from_slice(bytes.as_ref()).map_err(FetchRegistryError::Decode)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryDecodeError {
    pub url: &'static str,
    pub reason: String,
}

impl RegistryDecodeError {
    pub fn new(url: &'static str, reason: impl Into<String>) -> Self {
        Self {
            url,
            reason: reason.into(),
        }
    }

    fn from_json(error: serde_json::Error) -> Self {
        Self::new(REGISTRY_URL, error.to_string())
    }
}

impl fmt::Display for RegistryDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Failed to decode registry payload from {}: {}",
            self.url, self.reason
        )
    }
}

impl Error for RegistryDecodeError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentNotFoundError {
    pub agent_id: String,
}

impl AgentNotFoundError {
    pub fn new(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
        }
    }
}

impl fmt::Display for AgentNotFoundError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Agent with id \"{}\" was not found", self.agent_id)
    }
}

impl Error for AgentNotFoundError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedPlatformError {
    pub os: String,
    pub arch: String,
}

impl UnsupportedPlatformError {
    pub fn new(os: impl Into<String>, arch: impl Into<String>) -> Self {
        Self {
            os: os.into(),
            arch: arch.into(),
        }
    }
}

impl fmt::Display for UnsupportedPlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Unsupported platform: {}-{}", self.os, self.arch)
    }
}

impl Error for UnsupportedPlatformError {}

#[derive(Debug)]
pub enum FetchRegistryError {
    Request(reqwest::Error),
    Decode(RegistryDecodeError),
}

impl fmt::Display for FetchRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request(error) => write!(f, "Failed to fetch registry payload: {error}"),
            Self::Decode(error) => write!(f, "{error}"),
        }
    }
}

impl Error for FetchRegistryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Request(error) => Some(error),
            Self::Decode(error) => Some(error),
        }
    }
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
                .reason
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
