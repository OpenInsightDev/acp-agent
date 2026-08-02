//! YOLO-mode resolution for registry agents.
//!
//! ACP does not standardize a "yolo" mode: every agent names its
//! auto-approve-everything mode differently (Claude uses `bypassPermissions`,
//! Codex uses `agent-full-access`, Gemini uses `yolo`). This module embeds the
//! curated catalog from `data/yolo-modes.json` and resolves the correct
//! command-line flag (or a helpful protocol-level hint) for a given registry
//! agent id.
//!
//! The catalog stores only yolo-specific information — everything else (name,
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

use anyhow::{Result, anyhow};
use serde::Deserialize;

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

/// The embedded yolo-mode catalog keyed by registry agent id.
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

    /// Returns the catalog compiled into the binary from `data/yolo-modes.json`.
    ///
    /// Panics only when the embedded JSON fails to decode, which indicates a
    /// packaging error rather than a runtime condition.
    pub fn embedded() -> Self {
        Self::from_json(include_str!("../data/yolo-modes.json"))
            .expect("embedded data/yolo-modes.json must decode")
    }

    /// Looks up the yolo-mode mapping for a registry agent id.
    pub fn find(&self, agent_id: &str) -> Option<&YoloModeInfo> {
        self.agents.get(agent_id)
    }
}

/// Resolves the command-line arguments that activate yolo for an agent.
///
/// Returns the agent's startup flag (split into tokens so multi-token flags
/// such as `--permission-mode bypass` work) when one exists. For agents that
/// only support protocol-level yolo (`session/set_mode` or
/// `session/set_config_option`) or none at all, it returns an error with
/// guidance instead of silently skipping the requested auto-approve behavior.
pub fn yolo_extra_args(agent_id: &str) -> Result<Vec<String>> {
    let catalog = YoloModes::embedded();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_decodes() {
        let catalog = YoloModes::embedded();
        assert!(catalog.version >= 1);
        assert!(catalog.agents.len() >= 10);
        assert!(catalog.find("gemini").is_some());
    }

    #[test]
    fn single_token_flag_is_injected() {
        let args = yolo_extra_args("gemini").expect("gemini has a --yolo flag");
        assert_eq!(args, vec!["--yolo"]);
    }

    #[test]
    fn multi_token_flag_is_split() {
        let args = yolo_extra_args("devin").expect("devin has a permission-mode flag");
        assert_eq!(args, vec!["--permission-mode", "bypass"]);
    }

    #[test]
    fn set_mode_agent_without_flag_errors() {
        let error = yolo_extra_args("qwen-code").expect_err("qwen has no CLI yolo flag");
        assert!(error.to_string().contains("session/set_mode"));
        assert!(error.to_string().contains("yolo"));
    }

    #[test]
    fn config_option_agent_errors_with_selector() {
        let error = yolo_extra_args("amp-acp").expect_err("amp has no CLI yolo flag");
        let message = error.to_string();
        assert!(message.contains("config option permissions=bypass"));
        assert!(message.contains("session/set_config_option"));
    }

    #[test]
    fn agent_without_yolo_errors() {
        let error = yolo_extra_args("opencode").expect_err("opencode has no yolo mode");
        assert!(error.to_string().contains("no yolo mode"));
    }

    #[test]
    fn unknown_agent_errors() {
        let error = yolo_extra_args("not-a-real-agent").expect_err("unknown agent");
        assert!(error.to_string().contains("no yolo mode mapping"));
    }
}
