//! YOLO-mode resolution for registry agents.
//!
//! ACP does not standardize a "yolo" mode: every agent names its
//! auto-approve-everything mode differently (Claude uses `bypassPermissions`,
//! Codex uses `agent-full-access`, Gemini uses `yolo`). This module fetches the
//! curated catalog from the published CDN (mirroring how the registry itself is
//! fetched) and resolves the correct command-line flag — or a helpful
//! protocol-level hint — for a given registry agent id.
//!
//! The catalog stores only yolo-specific information; everything else (name,
//! description, distribution) already lives in the public ACP registry. Each
//! entry may carry any of:
//!
//! - `flag`: a startup flag that activates yolo, which `--yolo` injects;
//! - `mode`: the `modeId` accepted by ACP `session/set_mode`;
//! - `option`: a config-option selector for ACP `session/set_config_option`.
//!
//! An empty object means the agent is confirmed to have no yolo mode; an
//! absent entry means the mapping is unknown.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use anyhow::{Result, anyhow};
use serde::Deserialize;

/// CDN URL for the published yolo-mode catalog.
///
/// The catalog is maintained in the repository at `data/yolo-modes.json` and
/// served through jsDelivr's GitHub CDN, so it can be updated independently of
/// new CLI releases.
pub const YOLO_MODES_URL: &str =
    "https://cdn.jsdelivr.net/gh/OpenInsightDev/acp-agent@main/data/yolo-modes.json";

/// ACP config-option selector for agents whose yolo mode lives in
/// `session/set_config_option` rather than `session/set_mode`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct YoloConfigOption {
    /// Identifier of the config option (for example `"mode"` or `"permissions"`).
    #[serde(rename = "configId")]
    pub config_id: String,
    /// Value of that option that selects the yolo behavior, when known.
    #[serde(default, rename = "value")]
    pub value: Option<String>,
}

/// Minimal yolo-mode mapping for one registry agent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct YoloModeInfo {
    /// Startup flag (possibly with a value) that activates yolo, if any.
    #[serde(default, rename = "flag")]
    pub cli_flag: Option<String>,
    /// The `modeId` accepted by `session/set_mode`, if any.
    #[serde(default, rename = "mode")]
    pub mode_id: Option<String>,
    /// Config-option selector when yolo is set via `session/set_config_option`.
    #[serde(default, rename = "option")]
    pub config_option: Option<YoloConfigOption>,
}

impl YoloModeInfo {
    /// Returns `true` when the agent is confirmed to have no yolo mode.
    pub fn has_no_yolo(&self) -> bool {
        self.cli_flag.is_none() && self.mode_id.is_none() && self.config_option.is_none()
    }
}

/// The yolo-mode catalog keyed by registry agent id.
#[derive(Debug, Clone, Deserialize)]
pub struct YoloModes {
    /// Catalog schema version.
    pub version: u64,
    /// Agent id → yolo-mode mapping.
    pub agents: BTreeMap<String, YoloModeInfo>,
}

impl YoloModes {
    /// Decodes the catalog from an arbitrary JSON string.
    pub fn from_json(input: &str) -> Result<Self> {
        serde_json::from_str(input)
            .map_err(|error| anyhow!("failed to decode yolo-modes.json: {error}"))
    }

    /// Looks up the yolo-mode mapping for a registry agent id.
    pub fn find(&self, agent_id: &str) -> Option<&YoloModeInfo> {
        self.agents.get(agent_id)
    }
}

/// Downloads the yolo-mode catalog from [`YOLO_MODES_URL`].
///
/// The parsed catalog is cached for the process lifetime, so repeated
/// `--yolo` resolutions do not refetch or reparse the payload. Failures are
/// not cached, so a transient network error can be retried.
pub async fn fetch_yolo_modes() -> Result<YoloModes> {
    static CACHE: OnceLock<YoloModes> = OnceLock::new();

    if let Some(cached) = CACHE.get() {
        return Ok(cached.clone());
    }

    let response = reqwest::get(YOLO_MODES_URL)
        .await
        .map_err(|error| anyhow!("failed to fetch yolo-mode catalog: {error}"))?;
    let response = response
        .error_for_status()
        .map_err(|error| anyhow!("failed to fetch yolo-mode catalog: {error}"))?;
    let text = response
        .text()
        .await
        .map_err(|error| anyhow!("failed to fetch yolo-mode catalog: {error}"))?;

    let catalog = YoloModes::from_json(&text)?;
    let _ = CACHE.set(catalog.clone());
    Ok(catalog)
}

/// Resolves the command-line arguments that activate yolo for an agent.
///
/// Returns the agent's startup flag (split into tokens so multi-token flags
/// such as `--permission-mode bypass` work) when one exists. For agents that
/// only support protocol-level yolo (`session/set_mode` or
/// `session/set_config_option`) or none at all, it returns an error with
/// guidance instead of silently skipping the requested auto-approve behavior.
pub fn yolo_extra_args_from(catalog: &YoloModes, agent_id: &str) -> Result<Vec<String>> {
    let info = catalog.find(agent_id).ok_or_else(|| {
        anyhow!(
            "no yolo mode mapping known for agent \"{agent_id}\"; \
             add an entry to data/yolo-modes.json or pass the agent's own flag explicitly"
        )
    })?;

    if let Some(flag) = &info.cli_flag {
        return Ok(flag.split_whitespace().map(str::to_string).collect());
    }

    if let Some(mode_id) = &info.mode_id {
        return Err(anyhow!(
            "agent \"{agent_id}\" enables yolo via ACP session/set_mode (modeId \"{mode_id}\") \
             and exposes no CLI flag; run without --yolo and have the ACP client send \
             session/set_mode, or pass the agent's own flag manually"
        ));
    }

    if let Some(option) = &info.config_option {
        let value = option.value.as_deref().unwrap_or("<yolo value>");
        return Err(anyhow!(
            "agent \"{agent_id}\" enables yolo via ACP config option {}={value} and exposes \
             no CLI flag; run without --yolo and have the ACP client send \
             session/set_config_option, or pass the agent's own flag manually",
            option.config_id
        ));
    }

    Err(anyhow!("agent \"{agent_id}\" has no yolo mode"))
}

/// Fetches the catalog from the CDN and resolves the agent's yolo arguments.
pub async fn yolo_extra_args(agent_id: &str) -> Result<Vec<String>> {
    let catalog = fetch_yolo_modes().await?;
    yolo_extra_args_from(&catalog, agent_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "version": 1,
        "agents": {
            "gemini": { "mode": "yolo", "flag": "--yolo" },
            "devin": { "mode": "bypass", "flag": "--permission-mode bypass" },
            "qwen-code": { "mode": "yolo" },
            "amp-acp": { "option": { "configId": "permissions", "value": "bypass" } },
            "opencode": {}
        }
    }"#;

    fn sample_catalog() -> YoloModes {
        YoloModes::from_json(SAMPLE).expect("sample catalog should decode")
    }

    #[test]
    fn catalog_decodes() {
        let catalog = sample_catalog();
        assert!(catalog.version >= 1);
        assert!(catalog.find("gemini").is_some());
        assert!(catalog.find("missing").is_none());
    }

    #[test]
    fn single_token_flag_is_injected() {
        let args = yolo_extra_args_from(&sample_catalog(), "gemini").expect("gemini has a flag");
        assert_eq!(args, vec!["--yolo"]);
    }

    #[test]
    fn multi_token_flag_is_split() {
        let args = yolo_extra_args_from(&sample_catalog(), "devin").expect("devin has a flag");
        assert_eq!(args, vec!["--permission-mode", "bypass"]);
    }

    #[test]
    fn set_mode_agent_without_flag_errors() {
        let error = yolo_extra_args_from(&sample_catalog(), "qwen-code")
            .expect_err("qwen has no CLI yolo flag");
        assert!(error.to_string().contains("session/set_mode"));
        assert!(error.to_string().contains("yolo"));
    }

    #[test]
    fn config_option_agent_errors_with_selector() {
        let error = yolo_extra_args_from(&sample_catalog(), "amp-acp")
            .expect_err("amp has no CLI yolo flag");
        let message = error.to_string();
        assert!(message.contains("config option permissions=bypass"));
        assert!(message.contains("session/set_config_option"));
    }

    #[test]
    fn agent_without_yolo_errors() {
        let error = yolo_extra_args_from(&sample_catalog(), "opencode")
            .expect_err("opencode has no yolo mode");
        assert!(error.to_string().contains("no yolo mode"));
    }

    #[test]
    fn unknown_agent_errors() {
        let error =
            yolo_extra_args_from(&sample_catalog(), "not-a-real-agent").expect_err("unknown agent");
        assert!(error.to_string().contains("no yolo mode mapping"));
    }
}
