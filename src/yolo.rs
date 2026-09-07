//! YOLO-mode resolution for registry agents.
//!
//! ACP does not standardize a "yolo" mode: every agent names its
//! auto-approve-everything mode differently (Claude uses `bypassPermissions`,
//! Codex uses `agent-full-access`, Gemini uses `yolo`). This module resolves
//! the correct command-line arguments for a given registry agent id from a curated
//! catalog. The catalog is fetched from the published CDN (the update source)
//! and falls back to the copy bundled with this release when the network is
//! unavailable.
//!
//! The catalog stores only yolo-specific information; everything else (name,
//! description, distribution) already lives in the public ACP registry. Each
//! entry contains startup arguments that `--yolo` injects. Agents without a
//! supported startup argument mapping are not included in the catalog.

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde::Deserialize;

/// CDN URL for the published yolo-mode catalog.
///
/// The catalog is maintained in the repository at `data/yolo-modes.json` and
/// served through jsDelivr's GitHub CDN, so it can be updated independently of
/// new CLI releases. The CDN is the update source: [`fetch_yolo_modes`] tries
/// it first and falls back to [`EMBEDDED_YOLO_MODES`] when it is unreachable.
pub const YOLO_MODES_URL: &str =
    "https://cdn.jsdelivr.net/gh/OpenInsightDev/acp-agent@main/data/yolo-modes.json";

/// Yolo-mode catalog bundled with the release.
///
/// `data/yolo-modes.json` is embedded at compile time, so every released
/// binary (and Docker image) carries a local copy of the catalog. It is used
/// when the CDN is unreachable (offline, air-gapped, firewalled environments)
/// so `--yolo` never hard-fails on a missing network.
pub const EMBEDDED_YOLO_MODES: &str = include_str!("../data/yolo-modes.json");

// Bound remote lookup so offline use reaches the embedded catalog promptly.
const YOLO_MODES_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Startup arguments mapping for one registry agent.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct YoloMode {
    /// Arguments that activate yolo, preserving each argument boundary.
    pub args: Vec<String>,
}

/// The yolo-mode catalog keyed by registry agent id.
#[derive(Debug, Clone, Deserialize)]
pub struct YoloModes {
    /// Catalog schema version.
    pub version: u64,
    /// Agent id → yolo-mode mapping.
    pub agents: BTreeMap<String, YoloMode>,
}

impl YoloModes {
    /// Decodes the catalog from an arbitrary JSON string.
    pub fn from_json(input: &str) -> Result<Self> {
        serde_json::from_str(input)
            .map_err(|error| anyhow!("failed to decode yolo-modes.json: {error}"))
    }

    /// Looks up the yolo-mode mapping for a registry agent id.
    pub fn find(&self, agent_id: &str) -> Option<&YoloMode> {
        self.agents.get(agent_id)
    }
}

/// Resolves the yolo-mode catalog for `--yolo` resolution.
///
/// The catalog is resolved in this order:
/// 1. the published CDN catalog ([`YOLO_MODES_URL`]) — the update source;
/// 2. the catalog bundled with this release ([`EMBEDDED_YOLO_MODES`]).
///
/// The parsed catalog is cached for the process lifetime, so repeated
/// `--yolo` resolutions do not refetch or reparse the payload.
pub async fn fetch_yolo_modes() -> Result<YoloModes> {
    static CACHE: OnceLock<YoloModes> = OnceLock::new();

    if let Some(cached) = CACHE.get() {
        return Ok(cached.clone());
    }

    match fetch_remote_yolo_modes().await {
        Ok(catalog) => {
            let _ = CACHE.set(catalog.clone());
            Ok(catalog)
        }
        Err(error) => {
            eprintln!("warning: {error}; using the yolo-mode catalog bundled with this release");
            let catalog = embedded_yolo_modes().clone();
            let _ = CACHE.set(catalog.clone());
            Ok(catalog)
        }
    }
}

async fn fetch_remote_yolo_modes() -> Result<YoloModes> {
    let client = reqwest::Client::builder()
        .timeout(YOLO_MODES_FETCH_TIMEOUT)
        .build()
        .map_err(|error| anyhow!("failed to build HTTP client for yolo-mode catalog: {error}"))?;
    let response = client
        .get(YOLO_MODES_URL)
        .send()
        .await
        .map_err(|error| anyhow!("failed to fetch yolo-mode catalog: {error}"))?;
    let response = response
        .error_for_status()
        .map_err(|error| anyhow!("failed to fetch yolo-mode catalog: {error}"))?;
    let text = response
        .text()
        .await
        .map_err(|error| anyhow!("failed to fetch yolo-mode catalog: {error}"))?;
    YoloModes::from_json(&text)
}

fn embedded_yolo_modes() -> &'static YoloModes {
    static EMBEDDED: OnceLock<YoloModes> = OnceLock::new();
    EMBEDDED.get_or_init(|| {
        YoloModes::from_json(EMBEDDED_YOLO_MODES)
            .expect("embedded yolo-mode catalog (data/yolo-modes.json) must decode")
    })
}

/// Resolves the command-line arguments that activate yolo for an agent.
///
/// Returns the agent's startup arguments when a mapping exists.
pub fn yolo_extra_args_from(catalog: &YoloModes, agent_id: &str) -> Result<Vec<String>> {
    let info = catalog.find(agent_id).ok_or_else(|| {
        anyhow!(
            "no yolo mode mapping known for agent \"{agent_id}\"; \
             add an entry to data/yolo-modes.json or pass the agent's own arguments explicitly"
        )
    })?;

    Ok(info.args.clone())
}

/// Fetches the catalog from the CDN and resolves the agent's yolo arguments.
pub async fn yolo_extra_args(agent_id: &str) -> Result<Vec<String>> {
    let catalog = fetch_yolo_modes().await?;
    yolo_extra_args_from(&catalog, agent_id)
}

/// Prepends the agent's yolo startup arguments when `enabled` is true.
pub async fn resolve_args(agent_id: &str, enabled: bool, args: Vec<String>) -> Result<Vec<String>> {
    if !enabled {
        return Ok(args);
    }
    let extra = yolo_extra_args(agent_id).await?;
    Ok(extra.into_iter().chain(args).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "version": 2,
        "agents": {
            "gemini": { "args": ["--yolo"] },
            "devin": { "args": ["--permission-mode", "allow all tools"] }
        }
    }"#;

    fn sample_catalog() -> YoloModes {
        YoloModes::from_json(SAMPLE).expect("sample catalog should decode")
    }

    #[test]
    fn single_token_arguments_are_injected() {
        let args = yolo_extra_args_from(&sample_catalog(), "gemini").expect("gemini has arguments");
        assert_eq!(args, vec!["--yolo"]);
    }

    #[test]
    fn argument_boundaries_are_preserved() {
        let args = yolo_extra_args_from(&sample_catalog(), "devin").expect("devin has arguments");
        assert_eq!(args, vec!["--permission-mode", "allow all tools"]);
    }

    #[test]
    fn unsupported_protocol_fields_are_rejected() {
        let error = YoloModes::from_json(
            r#"{
                "version": 1,
                "agents": { "qwen-code": { "flag": "--yolo" } }
            }"#,
        )
        .expect_err("protocol-level yolo fields are unsupported");
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn agent_without_yolo_mapping_errors() {
        let error = yolo_extra_args_from(&sample_catalog(), "opencode")
            .expect_err("opencode has no yolo mapping");
        assert!(error.to_string().contains("no yolo mode mapping"));
    }

    #[test]
    fn unknown_agent_errors() {
        let error =
            yolo_extra_args_from(&sample_catalog(), "not-a-real-agent").expect_err("unknown agent");
        assert!(error.to_string().contains("no yolo mode mapping"));
    }

    #[test]
    fn embedded_catalog_decodes_and_covers_released_agents() {
        let catalog = embedded_yolo_modes();
        assert!(catalog.version >= 2);
        assert!(catalog.find("codex-acp").is_some());
        assert!(catalog.find("gemini").is_some());
        assert!(catalog.find("claude-acp").is_some());
    }

    #[test]
    fn embedded_catalog_resolves_arguments() {
        let args = yolo_extra_args_from(embedded_yolo_modes(), "codex-acp")
            .expect("codex-acp has yolo arguments");
        assert_eq!(args, vec!["--dangerously-skip-sandbox-and-permissions"]);
    }

    #[tokio::test]
    async fn disabled_resolution_preserves_arguments_without_catalog_lookup() {
        let args = vec!["--model".to_string(), "demo".to_string()];
        assert_eq!(
            resolve_args("unknown-agent", false, args.clone())
                .await
                .unwrap(),
            args
        );
    }
}
