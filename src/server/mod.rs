//! Named ACP servers with dynamically registered in-process agent routers.

mod client;
mod control;
mod daemon;
mod projection;
mod protocol;
mod routes;
mod validation;

pub use client::{list, register, registrations, start, status, stop, unregister};

use std::time::Duration;

use anyhow::Result;
use serde::Serialize;

use crate::serve::RouteConfig;

const DEFAULT_NAME: &str = "default";
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Runs the foreground Unix-socket server daemon used by `Commands::Daemon`.
pub async fn run() -> Result<()> {
    daemon::run().await
}

/// Options accepted by `server start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartOptions {
    /// Stable local name used by subsequent server commands.
    pub name: String,
    /// Address on which the named server listens.
    pub host: String,
    /// Port on which the named server listens.
    pub port: u16,
}

impl Default for StartOptions {
    fn default() -> Self {
        Self {
            name: DEFAULT_NAME.to_string(),
            host: "127.0.0.1".to_string(),
            port: 8010,
        }
    }
}

/// Options accepted by `server register`.
#[derive(Debug, Clone)]
pub struct RegisterOptions {
    /// Target named server.
    pub name: String,
    /// Public route prefix; defaults to `/<agent-id>`.
    pub route: Option<String>,
    /// Shared ACP route configuration.
    pub config: RouteConfig,
    /// Whether to inject the agent's yolo argument.
    pub yolo: bool,
    /// Arguments forwarded to the agent process on connection.
    pub args: Vec<String>,
}

/// Stable JSON record for one live named server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServerRecord {
    /// Stable server name.
    pub name: String,
    /// Current live instance state.
    pub state: String,
    /// Configured listener host.
    pub host: String,
    /// Bound listener port.
    pub port: u16,
    /// Public HTTP address.
    pub address: String,
}

/// Stable JSON record for one agent registration (`server registrations`).
#[derive(Debug, Clone, Serialize)]
pub struct RegistrationRecord {
    /// Registry agent identifier.
    pub id: String,
    /// Public route prefix.
    pub route: String,
    /// Readiness state reported by the daemon's route runtime.
    pub readiness: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional readiness failure detail.
    pub detail: Option<String>,
}

/// Result of starting a named server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartResult {
    /// Stable server name.
    pub name: String,
    /// Public HTTP address.
    pub address: String,
}

/// Result of stopping a named server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopResult {
    /// Stable server name.
    pub name: String,
}

/// Result of registering an agent route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterResult {
    /// Registry agent identifier.
    pub agent_id: String,
    /// Registered public route prefix.
    pub route: String,
    /// Named server public address.
    pub address: String,
}

/// Result of unregistering an agent route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnregisterResult {
    /// Registry agent identifier.
    pub agent_id: String,
    /// Stable server name.
    pub server_name: String,
}

#[cfg(test)]
mod tests {
    use super::{ServerRecord, StartOptions};
    use crate::server::{
        routes::route_matches,
        validation::{validate_name, validate_route},
    };

    #[test]
    fn start_options_default_to_the_cli_server_defaults() {
        assert_eq!(
            StartOptions::default(),
            StartOptions {
                name: "default".into(),
                host: "127.0.0.1".into(),
                port: 8010,
            }
        );
    }

    #[test]
    fn server_record_serializes_only_live_fields() {
        let record = ServerRecord {
            name: "work".into(),
            state: "running".into(),
            host: "127.0.0.1".into(),
            port: 8010,
            address: "http://127.0.0.1:8010".into(),
        };
        let value = serde_json::to_value(record).unwrap();
        assert_eq!(
            value
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["name", "state", "host", "port", "address"]
        );
    }

    #[test]
    fn validates_server_names_and_agent_routes() {
        assert!(validate_name("default").is_ok());
        assert!(validate_name("team.one").is_ok());
        assert!(validate_name("../bad").is_err());
        assert!(validate_route("/codex-acp").is_ok());
        assert!(validate_route("/team/codex").is_ok());
        assert!(validate_route("/").is_err());
        assert!(validate_route("/custom/route").is_ok());
        assert!(validate_route("/bad/").is_err());
        assert!(validate_route("/bad path").is_err());
    }

    #[test]
    fn route_matching_uses_complete_path_segments() {
        assert!(route_matches("/agent", "/agent"));
        assert!(route_matches("/agent", "/agent/acp"));
        assert!(!route_matches("/agent", "/agent-two/acp"));
    }
}
