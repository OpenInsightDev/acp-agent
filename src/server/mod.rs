//! Named ACP servers with dynamically registered in-process agent routers.

mod client;
mod daemon;
mod state;

pub use client::{list, logs, register, registrations, start, status, stop, unregister};
pub use daemon::run;
use state::*;

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::{State, rejection::JsonRejection},
    http::{Request, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    net::TcpListener,
    process::Command,
    sync::{RwLock, watch},
    time::{sleep, timeout},
};
use tower::ServiceExt;

const DEFAULT_NAME: &str = "default";
const START_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
const SERVER_PROTOCOL_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_LOG_TAIL_BYTES: usize = 1024 * 1024;

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
    /// ACP endpoint path below the public route.
    pub path: String,
    /// Browser origins allowed by the agent router.
    pub cors_origins: Vec<String>,
    /// Whether all browser origins are accepted.
    pub allow_any_origin: bool,
    /// Whether the agent router exposes `/health`.
    pub health_endpoint: bool,
    /// Whether the agent router exposes `/readyz`.
    pub readyz_endpoint: bool,
    /// Maximum number of concurrent agent processes for this route.
    pub max_processes: usize,
    /// Whether to inject the agent's yolo argument.
    pub yolo: bool,
    /// Arguments forwarded to the agent process on connection.
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ServerStatus {
    name: String,
    pid: u32,
    version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistrationInfo {
    id: String,
    route: String,
    readyz_endpoint: bool,
}

// Lifecycle combines control-endpoint reachability with process existence so
// callers can distinguish a starting daemon from stale state left by a crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerRunState {
    Running,
    Starting,
    Stale,
    Stopped,
}

impl ServerRunState {
    fn as_str(self) -> &'static str {
        match self {
            ServerRunState::Running => "running",
            ServerRunState::Starting => "starting",
            ServerRunState::Stale => "stale",
            ServerRunState::Stopped => "stopped",
        }
    }
}

/// Stable JSON record for one named server (`server list` / `server status`).
///
/// Field order is significant: it defines the deterministic JSON output.
#[derive(Debug, Clone, Serialize)]
pub struct ServerRecord {
    /// Stable server name.
    pub name: String,
    /// Lifecycle state: running, starting, stale, or stopped.
    pub state: String,
    /// Configured listener host.
    pub listen_host: Option<String>,
    /// Bound listener port.
    pub port: Option<u16>,
    /// Public HTTP address.
    pub address: Option<String>,
    /// Daemon process identifier.
    pub pid: Option<u32>,
    /// Daemon protocol version.
    pub version: Option<String>,
}

/// Stable JSON record for one agent registration (`server registrations`).
#[derive(Debug, Clone, Serialize)]
pub struct RegistrationRecord {
    /// Registry agent identifier.
    pub id: String,
    /// Public route prefix.
    pub route: String,
    /// Probed readiness state.
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

/// Bounded log tail for a named server.
#[derive(Debug, Clone, Serialize)]
pub struct LogRecord {
    /// Stable server name.
    pub name: String,
    /// Requested tail lines in chronological order.
    pub lines: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentRegistrationRequest {
    id: String,
    route: String,
    serve: AgentServeRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentServeRequest {
    path: String,
    #[serde(default)]
    cors_origins: Vec<String>,
    #[serde(default)]
    allow_any_origin: bool,
    #[serde(default = "default_true")]
    health_endpoint: bool,
    #[serde(default = "default_true")]
    readyz_endpoint: bool,
    #[serde(default = "default_max_processes")]
    max_processes: usize,
    #[serde(default)]
    yolo: bool,
    #[serde(default)]
    args: Vec<String>,
}

fn default_true() -> bool {
    true
}

fn default_max_processes() -> usize {
    crate::serve::DEFAULT_MAX_PROCESSES
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentSelector {
    id: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct ApiError {
    error: String,
    message: String,
}

fn api_error(status: StatusCode, error: impl Into<String>, message: impl Into<String>) -> Response {
    (
        status,
        Json(ApiError {
            error: error.into(),
            message: message.into(),
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::client::*;
    use super::daemon::*;
    use axum::routing::any;
    use serde_json::Value;
    use std::net::SocketAddr;

    use super::*;

    #[test]
    fn tails_only_the_last_lines() {
        assert_eq!(tail_lines("a\nb\nc\n", 2), vec!["b", "c"]);
        assert_eq!(tail_lines("a\n", 5), vec!["a"]);
        assert_eq!(tail_lines("", 5), Vec::<&str>::new());
        assert_eq!(tail_lines("a\nb\n", 0), Vec::<&str>::new());
        assert_eq!(tail_lines("a\r\nb\r\nc\r\n", 2), vec!["b", "c"]);
    }

    #[tokio::test]
    async fn reads_only_the_requested_log_tail() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("server.log");
        let content = format!("first\n{}second\nthird\nfourth\n", "x".repeat(9000));
        tokio::fs::write(&path, content).await.unwrap();

        let content = read_log_tail(&path, 2).await.unwrap();
        assert_eq!(tail_lines(&content, 2), vec!["third", "fourth"]);
        assert!(!content.contains("first"));
        assert!(read_log_tail(&path, 0).await.unwrap().is_empty());
        assert_eq!(
            read_log_tail(&temporary.path().join("missing.log"), 0)
                .await
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );

        let oversized = temporary.path().join("oversized.log");
        tokio::fs::write(&oversized, vec![b'x'; MAX_LOG_TAIL_BYTES + 1])
            .await
            .unwrap();
        assert_eq!(
            read_log_tail(&oversized, 1).await.unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[tokio::test]
    async fn distinguishes_starting_from_stale_state() {
        let dead_endpoint = ServerFile {
            name: "test".into(),
            listen_host: "127.0.0.1".into(),
            port: 1,
            control_url: "http://127.0.0.1:1".into(),
            pid: std::process::id(),
            version: SERVER_PROTOCOL_VERSION.into(),
        };
        assert_eq!(
            observed_state(&dead_endpoint).await,
            ServerRunState::Starting
        );

        let stale = ServerFile {
            pid: 999_999_999,
            ..dead_endpoint
        };
        assert_eq!(observed_state(&stale).await, ServerRunState::Stale);
    }

    #[tokio::test]
    async fn status_reports_stopped_for_missing_state_and_json_is_deterministic() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = ServerPaths::new(temporary.path().join("servers")).unwrap();

        write_private_json_atomic(
            &paths.state_file("work"),
            &ServerFile {
                name: "work".into(),
                listen_host: "0.0.0.0".into(),
                port: 8020,
                control_url: "http://127.0.0.1:8020".into(),
                pid: 999_999_998,
                version: SERVER_PROTOCOL_VERSION.into(),
            },
        )
        .unwrap();
        std::fs::write(paths.log_file("work"), b"ignored\n").unwrap();

        let records = list_with_paths(&paths).await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "work");
        assert_eq!(records[0].state, "stale");
        assert_eq!(records[0].port, Some(8020));
        assert_eq!(records[0].address.as_deref(), Some("http://127.0.0.1:8020"));
        let serialized = serde_json::to_string_pretty(&records).unwrap();
        let name_pos = serialized.find("\"name\"").unwrap();
        let state_pos = serialized.find("\"state\"").unwrap();
        let host_pos = serialized.find("\"listen_host\"").unwrap();
        assert!(name_pos < state_pos && state_pos < host_pos);

        let status = status_with_paths(&paths, "missing").await.unwrap();
        assert_eq!(status.name, "missing");
        assert_eq!(status.state, "stopped");
        assert!(status.pid.is_none());
    }

    #[tokio::test]
    async fn reports_corrupt_state_instead_of_stopped_or_omitting_it() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = ServerPaths::new(temporary.path().join("servers")).unwrap();
        std::fs::write(paths.state_file("broken"), b"{not json").unwrap();

        let error = status_with_paths(&paths, "broken").await.unwrap_err();
        assert!(format!("{error:#}").contains("failed to inspect server state"));

        let error = list_with_paths(&paths).await.unwrap_err();
        assert!(format!("{error:#}").contains("failed to inspect server state"));
    }

    #[test]
    fn validates_names_and_routes() {
        assert!(validate_name("default").is_ok());
        assert!(validate_name("team.one").is_ok());
        assert!(validate_name("../bad").is_err());
        assert!(validate_route("/codex-acp").is_ok());
        assert!(validate_route("/team/codex").is_ok());
        assert!(validate_route("/").is_err());
        assert!(validate_route("/api/agents").is_err());
        assert!(validate_route("/bad/").is_err());
        assert!(validate_route("/bad path").is_err());
    }

    #[test]
    fn matches_complete_segments_only() {
        assert!(route_matches("/agent", "/agent"));
        assert!(route_matches("/agent", "/agent/acp"));
        assert!(!route_matches("/agent", "/agent-two/acp"));
    }

    #[test]
    fn rewrites_route_prefix_and_preserves_query() {
        let request = Request::builder()
            .uri("/codex-acp/acp?mode=test")
            .body(Body::empty())
            .unwrap();
        let request = rewrite_route_prefix(request, "/codex-acp").unwrap();
        assert_eq!(
            request.uri().path_and_query().unwrap().as_str(),
            "/acp?mode=test"
        );

        let request = Request::builder()
            .uri("/codex-acp?mode=test")
            .body(Body::empty())
            .unwrap();
        let request = rewrite_route_prefix(request, "/codex-acp").unwrap();
        assert_eq!(
            request.uri().path_and_query().unwrap().as_str(),
            "/?mode=test"
        );
    }

    #[test]
    fn formats_control_urls_for_wildcard_and_ipv6_listeners() {
        assert_eq!(
            control_url("0.0.0.0", 8010).unwrap(),
            "http://127.0.0.1:8010"
        );
        assert_eq!(control_url("::", 8010).unwrap(), "http://[::1]:8010");
        assert_eq!(control_url("::1", 8010).unwrap(), "http://[::1]:8010");
        assert_eq!(public_url("::1", 8010).unwrap(), "http://[::1]:8010");
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("localhost"));
        assert!(!is_loopback_host("0.0.0.0"));
    }

    #[test]
    fn formats_structured_and_unstructured_api_errors_for_cli_output() {
        assert_eq!(
            api_error_detail(
                r#"{"error":"route_conflict","message":"route /demo is already registered"}"#
            ),
            "route_conflict: route /demo is already registered"
        );
        assert_eq!(
            api_error_detail("upstream unavailable"),
            "upstream unavailable"
        );
        assert_eq!(api_error_detail("  "), "server returned no error details");
    }

    #[cfg(unix)]
    #[test]
    fn secures_state_directory_log_and_atomic_state_file() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("servers");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        let paths = ServerPaths::new(directory).unwrap();
        assert_eq!(
            std::fs::metadata(&paths.directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        let log_path = paths.log_file("test");
        std::fs::write(&log_path, b"old log").unwrap();
        std::fs::set_permissions(&log_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        drop(open_private_file(&log_path, true).unwrap());
        assert_eq!(
            std::fs::metadata(&log_path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let state_path = paths.state_file("test");
        let state = ServerFile {
            name: "test".into(),
            listen_host: "127.0.0.1".into(),
            port: 8010,
            control_url: "http://127.0.0.1:8010".into(),
            pid: 1,
            version: SERVER_PROTOCOL_VERSION.into(),
        };
        write_private_json_atomic(&state_path, &state).unwrap();
        assert_eq!(
            std::fs::metadata(&state_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let decoded: ServerFile =
            serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(decoded.name, "test");
        assert!(std::fs::read_dir(&paths.directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_start_reaps_child_and_removes_state() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let paths = ServerPaths::new(temporary.path().join("servers")).unwrap();
        let script = temporary.path().join("hang.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho $$ > \"$ACP_AGENT_SERVER_STATE_DIR/child.pid\"\nprintf '{\"bad\":true}\\n' > \"$ACP_AGENT_SERVER_STATE_DIR/timeout-test.json\"\nexec /bin/sleep 10\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();

        let error = start_with(
            StartOptions {
                name: "timeout-test".into(),
                host: "127.0.0.1".into(),
                port: 0,
            },
            paths.clone(),
            script,
            Duration::from_millis(500),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("timed out waiting"));
        assert!(!paths.state_file("timeout-test").exists());

        let pid = std::fs::read_to_string(paths.directory.join("child.pid")).unwrap();
        let alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "timed-out daemon process {pid} was not reaped");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_start_reaps_child_and_removes_state() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let paths = ServerPaths::new(temporary.path().join("servers")).unwrap();
        let script = temporary.path().join("cancelled-start.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho $$ > \"$ACP_AGENT_SERVER_STATE_DIR/cancelled.pid\"\nprintf '{\"bad\":true}\\n' > \"$ACP_AGENT_SERVER_STATE_DIR/cancelled-start.json\"\nexec /bin/sleep 60\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();

        let pid_path = paths.directory.join("cancelled.pid");
        let state_path = paths.state_file("cancelled-start");
        let task = tokio::spawn(start_with(
            StartOptions {
                name: "cancelled-start".into(),
                host: "127.0.0.1".into(),
                port: 0,
            },
            paths,
            script,
            Duration::from_secs(30),
        ));
        timeout(Duration::from_secs(2), async {
            while !pid_path.exists() || !state_path.exists() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("server child did not reach startup wait");
        let pid: libc::pid_t = std::fs::read_to_string(&pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        task.abort();
        assert!(task.await.is_err());
        timeout(Duration::from_secs(5), async {
            while state_path.exists() || unsafe { libc::kill(pid, 0) == 0 } {
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("cancelled server startup was not cleaned up");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn early_daemon_exit_reports_log_and_leaves_no_state() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let paths = ServerPaths::new(temporary.path().join("servers")).unwrap();
        let script = temporary.path().join("exit.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 7\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let error = start_with(
            StartOptions {
                name: "exit-test".into(),
                host: "127.0.0.1".into(),
                port: 0,
            },
            paths.clone(),
            script,
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("inspect"));
        assert!(!paths.state_file("exit-test").exists());
    }

    #[tokio::test]
    async fn port_zero_records_actual_port_and_shutdown_removes_state() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = ServerPaths::new(temporary.path().join("servers")).unwrap();
        let state_path = paths.state_file("ephemeral");
        let task = tokio::spawn(run_with(
            "ephemeral".into(),
            "127.0.0.1".into(),
            0,
            paths,
            Duration::from_millis(50),
        ));
        let state = timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(state) = read_json::<ServerFile>(&state_path).await {
                    break state;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_ne!(state.port, 0);
        assert_eq!(
            state.control_url,
            format!("http://127.0.0.1:{}", state.port)
        );
        let status = reqwest::Client::new()
            .post(format!("{}/api/shutdown", state.control_url))
            .send()
            .await
            .unwrap();
        assert_eq!(status.status(), StatusCode::ACCEPTED);
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!state_path.exists());
    }

    #[tokio::test]
    async fn wildcard_and_ipv6_listeners_publish_reachable_control_urls() {
        for (index, host) in ["0.0.0.0", "::1", "::"].into_iter().enumerate() {
            let Ok(probe) = TcpListener::bind((host, 0)).await else {
                continue;
            };
            drop(probe);
            let temporary = tempfile::tempdir().unwrap();
            let paths = ServerPaths::new(temporary.path().join("servers")).unwrap();
            let name = format!("address-{index}");
            let state_path = paths.state_file(&name);
            let task = tokio::spawn(run_with(
                name,
                host.into(),
                0,
                paths,
                Duration::from_millis(50),
            ));
            let state = timeout(Duration::from_secs(2), async {
                loop {
                    if let Ok(state) = read_json::<ServerFile>(&state_path).await {
                        break state;
                    }
                    sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(state.control_url, control_url(host, state.port).unwrap());
            let status = reqwest::Client::new()
                .get(format!("{}/api/status", state.control_url))
                .send()
                .await
                .unwrap();
            assert_eq!(status.status(), StatusCode::OK);
            reqwest::Client::new()
                .post(format!("{}/api/shutdown", state.control_url))
                .send()
                .await
                .unwrap();
            timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(!state_path.exists());
        }
    }

    #[tokio::test]
    async fn bind_failure_does_not_write_state() {
        let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = occupied.local_addr().unwrap().port();
        let temporary = tempfile::tempdir().unwrap();
        let paths = ServerPaths::new(temporary.path().join("servers")).unwrap();
        let error = run_with(
            "occupied".into(),
            "127.0.0.1".into(),
            port,
            paths.clone(),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("failed to bind named server"));
        assert!(!paths.state_file("occupied").exists());
    }

    fn test_state() -> ServerState {
        let (shutdown, _) = watch::channel(false);
        let (cancel, _) = watch::channel(false);
        ServerState {
            server_name: "test".to_string(),
            agents: Arc::default(),
            shutdown,
            cancel,
        }
    }

    fn echo_router() -> Router {
        Router::new().fallback(any(|request: Request<Body>| async move {
            request
                .uri()
                .path_and_query()
                .map_or("/", |value| value.as_str())
                .to_string()
        }))
    }

    async fn spawn_axum(app: Router) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (address, task)
    }

    #[tokio::test]
    async fn readiness_requires_status_to_match_the_state_file() {
        let (address, task) = spawn_axum(server_router(test_state())).await;
        let mut state = ServerFile {
            name: "test".into(),
            listen_host: "127.0.0.1".into(),
            port: address.port(),
            control_url: format!("http://{address}"),
            pid: 1,
            version: SERVER_PROTOCOL_VERSION.into(),
        };
        assert!(server_is_alive(&state).await);
        state.name = "other".into();
        assert!(!server_is_alive(&state).await);
        state.name = "test".into();
        state.version = "incompatible".into();
        assert!(!server_is_alive(&state).await);
        task.abort();
    }

    #[tokio::test]
    async fn readiness_accepts_a_running_server_from_an_older_release() {
        let version = "0.0.4";
        let router = Router::new().route(
            "/api/status",
            get(move || async move {
                Json(ServerStatus {
                    name: "test".into(),
                    pid: 1,
                    version: version.into(),
                })
            }),
        );
        let (address, task) = spawn_axum(router).await;
        let state = ServerFile {
            name: "test".into(),
            listen_host: "127.0.0.1".into(),
            port: address.port(),
            control_url: format!("http://{address}"),
            pid: 1,
            version: version.into(),
        };

        assert!(server_is_alive(&state).await);
        task.abort();
    }

    #[tokio::test]
    async fn shutdown_force_closes_long_stream_after_signal_grace() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/stream",
            get(|| async {
                Body::from_stream(futures::stream::pending::<
                    std::result::Result<axum::body::Bytes, std::io::Error>,
                >())
            }),
        );
        let (shutdown, receiver) = watch::channel(false);
        let task = tokio::spawn(serve_with_shutdown(
            listener,
            router,
            receiver,
            Duration::from_millis(50),
        ));
        let response = reqwest::Client::new()
            .get(format!("http://{address}/stream"))
            .send()
            .await
            .unwrap();

        sleep(Duration::from_millis(100)).await;
        assert!(
            !task.is_finished(),
            "shutdown grace started before its signal"
        );
        let signaled_at = tokio::time::Instant::now();
        shutdown.send_replace(true);
        timeout(Duration::from_secs(1), task)
            .await
            .expect("server did not stop within its bounded grace")
            .unwrap()
            .unwrap();
        assert!(signaled_at.elapsed() >= Duration::from_millis(40));
        drop(response);
    }

    #[tokio::test]
    async fn dispatches_the_longest_matching_router_and_preserves_the_suffix() {
        let state = test_state();
        insert_agent(
            &state,
            RegisteredAgent::new("root".into(), "/agent".into(), echo_router()),
        )
        .await
        .unwrap();
        insert_agent(
            &state,
            RegisteredAgent::new(
                "nested".into(),
                "/agent/child".into(),
                Router::new().fallback(any(|| async { "nested" })),
            ),
        )
        .await
        .unwrap();
        let (address, task) = spawn_axum(server_router(state)).await;
        let client = reqwest::Client::new();

        let response = client
            .get(format!("http://{address}/agent/acp?mode=test"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), "/acp?mode=test");
        let response = client
            .get(format!("http://{address}/agent/child/acp"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), "nested");
        assert_eq!(
            client
                .get(format!("http://{address}/agent-two/acp"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        task.abort();
    }

    #[tokio::test]
    async fn dispatch_preserves_method_headers_body_query_and_extensions() {
        #[derive(Clone)]
        struct Marker(&'static str);

        let target = Router::new().fallback(any(|request: Request<Body>| async move {
            let method = request.method().clone();
            let uri = request.uri().clone();
            let header = request
                .headers()
                .get("x-dispatch-test")
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            let marker = request.extensions().get::<Marker>().unwrap().0;
            let body = axum::body::to_bytes(request.into_body(), 1024)
                .await
                .unwrap();
            format!(
                "{method} {} {header} {marker} {}",
                uri.path_and_query().unwrap(),
                String::from_utf8(body.to_vec()).unwrap()
            )
        }));
        let state = test_state();
        insert_agent(
            &state,
            RegisteredAgent::new("demo".into(), "/demo".into(), target),
        )
        .await
        .unwrap();
        let mut request = Request::builder()
            .method("PATCH")
            .uri("/demo/rpc?mode=test")
            .header("x-dispatch-test", "header-ok")
            .body(Body::from("body-ok"))
            .unwrap();
        request.extensions_mut().insert(Marker("extension-ok"));

        let response = dispatch_agent(State(state), request).await;
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            "PATCH /rpc?mode=test header-ok extension-ok body-ok"
        );
    }

    #[tokio::test]
    async fn rejects_duplicate_agent_ids_and_routes() {
        let state = test_state();
        insert_agent(
            &state,
            RegisteredAgent::new("demo".into(), "/demo".into(), echo_router()),
        )
        .await
        .unwrap();
        let duplicate_id = insert_agent(
            &state,
            RegisteredAgent::new("demo".into(), "/other".into(), echo_router()),
        )
        .await
        .unwrap_err();
        assert_eq!(duplicate_id.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(duplicate_id.into_body(), 1024)
            .await
            .unwrap();
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"], "agent_id_conflict");
        let duplicate_route = insert_agent(
            &state,
            RegisteredAgent::new("other".into(), "/demo".into(), echo_router()),
        )
        .await
        .unwrap_err();
        assert_eq!(duplicate_route.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(duplicate_route.into_body(), 1024)
            .await
            .unwrap();
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"], "route_conflict");
    }

    #[tokio::test]
    async fn management_api_is_unauthenticated_and_returns_json_errors() {
        let (address, task) = spawn_axum(server_router(test_state())).await;
        let client = reqwest::Client::new();
        let status = client
            .get(format!("http://{address}/api/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(status.status(), StatusCode::OK);
        let status: ServerStatus = status.json().await.unwrap();
        assert_eq!(status.name, "test");
        assert_eq!(status.version, SERVER_PROTOCOL_VERSION);

        let invalid = client
            .post(format!("http://{address}/api/agents"))
            .json(&serde_json::json!({
                "id": "demo",
                "route": "/api/demo",
                "serve": { "path": "/acp" }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        let error: Value = invalid.json().await.unwrap();
        assert_eq!(error["error"], "invalid_route");

        let malformed = client
            .post(format!("http://{address}/api/agents"))
            .header("content-type", "application/json")
            .body("{")
            .send()
            .await
            .unwrap();
        assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
        let error: Value = malformed.json().await.unwrap();
        assert_eq!(error["error"], "invalid_request");

        let legacy_proxy_request = client
            .post(format!("http://{address}/api/agents"))
            .json(&serde_json::json!({
                "id": "demo",
                "route": "/demo",
                "target": "http://127.0.0.1:9000",
                "pid": 123
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(legacy_proxy_request.status(), StatusCode::BAD_REQUEST);
        let error: Value = legacy_proxy_request.json().await.unwrap();
        assert_eq!(error["error"], "invalid_request");

        let invalid_options = client
            .post(format!("http://{address}/api/agents"))
            .json(&serde_json::json!({
                "id": "demo",
                "route": "/demo",
                "serve": { "path": "acp" }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(invalid_options.status(), StatusCode::BAD_REQUEST);
        let error: Value = invalid_options.json().await.unwrap();
        assert_eq!(error["error"], "invalid_options");

        let conflicting_cors = client
            .post(format!("http://{address}/api/agents"))
            .json(&serde_json::json!({
                "id": "demo",
                "route": "/demo",
                "serve": {
                    "path": "/acp",
                    "cors_origins": ["https://example.com"],
                    "allow_any_origin": true
                }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(conflicting_cors.status(), StatusCode::BAD_REQUEST);
        let error: Value = conflicting_cors.json().await.unwrap();
        assert_eq!(error["error"], "invalid_options");

        let missing = client
            .delete(format!("http://{address}/api/agents"))
            .json(&serde_json::json!({ "id": "demo" }))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let error: Value = missing.json().await.unwrap();
        assert_eq!(error["error"], "agent_not_found");
        task.abort();
    }

    #[tokio::test]
    async fn registrations_endpoint_reports_routes_and_flags() {
        let state = test_state();
        insert_agent(
            &state,
            RegisteredAgent {
                id: "demo".into(),
                route: "/demo".into(),
                router: echo_router(),
                readyz_endpoint: false,
            },
        )
        .await
        .unwrap();
        insert_agent(
            &state,
            RegisteredAgent::new("zulu".into(), "/zulu".into(), echo_router()),
        )
        .await
        .unwrap();
        let (address, task) = spawn_axum(server_router(state)).await;
        let response = reqwest::get(format!("http://{address}/api/registrations"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let registrations: Value = response.json().await.unwrap();
        assert_eq!(registrations.as_array().unwrap().len(), 2);
        assert_eq!(registrations[0]["route"], "/demo");
        assert_eq!(registrations[0]["id"], "demo");
        assert_eq!(registrations[0]["readyz_endpoint"], false);
        assert_eq!(registrations[1]["route"], "/zulu");
        assert_eq!(registrations[1]["readyz_endpoint"], true);
        task.abort();
    }

    #[tokio::test]
    async fn readiness_probe_distinguishes_ready_and_disabled() {
        let (address, task) = spawn_axum(server_router(test_state())).await;
        let state = ServerFile {
            name: "test".into(),
            listen_host: "127.0.0.1".into(),
            port: address.port(),
            control_url: format!("http://{address}"),
            pid: 1,
            version: SERVER_PROTOCOL_VERSION.into(),
        };
        let (readiness, detail) = probe_registration_readiness(
            &state,
            &RegistrationInfo {
                id: "demo".into(),
                route: "/demo".into(),
                readyz_endpoint: false,
            },
        )
        .await;
        assert_eq!(readiness, "disabled");
        assert!(detail.is_none());
        task.abort();
    }

    #[cfg(unix)]
    mod acp_network {
        use agent_client_protocol::AcpAgentConfig;
        use async_tungstenite::tokio::connect_async;
        use async_tungstenite::tungstenite::Message;
        use futures::StreamExt;
        use reqwest::header::{ACCEPT, CONTENT_TYPE};
        use serde_json::{Value, json};

        use super::*;

        const CONNECTION_ID: &str = "acp-connection-id";
        const INITIALIZE_REQUEST: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}"#;
        const ECHO_REQUEST: &str = r#"{"jsonrpc":"2.0","id":2,"method":"test/echo","params":{}}"#;

        fn fixture_agent() -> AcpAgentConfig {
            AcpAgentConfig::new("/bin/sh").args([
                "-c",
                r#"while IFS= read -r line; do
case "$line" in
*'"id":2'*)
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"echo":"ok"}}'
;;
*)
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}'
;;
esac
done"#,
            ])
        }

        struct NamedAcpServer {
            address: SocketAddr,
            task: Option<tokio::task::JoinHandle<Result<()>>>,
        }

        impl NamedAcpServer {
            async fn start(shutdown_grace: Duration) -> Self {
                Self::start_with_agent(fixture_agent(), shutdown_grace).await
            }

            async fn start_with_agent(config: AcpAgentConfig, shutdown_grace: Duration) -> Self {
                let (shutdown, receiver) = watch::channel(false);
                let (cancel, cancel_rx) = watch::channel(false);
                let state = ServerState {
                    server_name: "test".into(),
                    agents: Arc::default(),
                    shutdown,
                    cancel: cancel.clone(),
                };
                let router = crate::serve::agent_router(
                    config,
                    &crate::serve::AgentRouterOptions::default(),
                    cancel_rx,
                )
                .unwrap();
                insert_agent(
                    &state,
                    RegisteredAgent::new("demo".into(), "/demo".into(), router),
                )
                .await
                .unwrap();
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let task = tokio::spawn(crate::serve::serve_with_shutdown(
                    listener,
                    server_router(state),
                    receiver,
                    cancel,
                    shutdown_grace,
                ));
                Self {
                    address,
                    task: Some(task),
                }
            }

            fn http_url(&self, path: &str) -> String {
                format!("http://{}{path}", self.address)
            }

            fn ws_url(&self, path: &str) -> String {
                format!("ws://{}{path}", self.address)
            }

            async fn stop(mut self) {
                reqwest::Client::new()
                    .post(self.http_url("/api/shutdown"))
                    .send()
                    .await
                    .unwrap();
                self.wait().await;
            }

            async fn wait(&mut self) {
                timeout(Duration::from_secs(2), self.task.take().unwrap())
                    .await
                    .expect("named ACP server shutdown timed out")
                    .unwrap()
                    .unwrap();
            }
        }

        impl Drop for NamedAcpServer {
            fn drop(&mut self) {
                if let Some(task) = &self.task {
                    task.abort();
                }
            }
        }

        async fn initialize_http(client: &reqwest::Client, endpoint: &str) -> reqwest::Response {
            timeout(
                Duration::from_secs(5),
                client
                    .post(endpoint)
                    .header(CONTENT_TYPE, "application/json")
                    .body(INITIALIZE_REQUEST)
                    .send(),
            )
            .await
            .expect("HTTP initialize timed out")
            .unwrap()
        }

        async fn initialize_websocket(
            socket: &mut async_tungstenite::WebSocketStream<
                async_tungstenite::tokio::TokioAdapter<tokio::net::TcpStream>,
            >,
        ) {
            socket
                .send(Message::Text(INITIALIZE_REQUEST.into()))
                .await
                .unwrap();
            let frame = timeout(Duration::from_secs(5), socket.next())
                .await
                .expect("WebSocket initialize timed out")
                .unwrap()
                .unwrap();
            let Message::Text(text) = frame else {
                panic!("expected text initialize response, got {frame:?}");
            };
            let response: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(response["result"]["protocolVersion"], json!(1));
        }

        #[tokio::test]
        async fn named_route_serves_http_sse_lifecycle_then_unregisters() {
            let server = NamedAcpServer::start(Duration::from_millis(50)).await;
            let client = reqwest::Client::new();
            let endpoint = server.http_url("/demo/acp");
            let health = client
                .get(server.http_url("/demo/health"))
                .send()
                .await
                .unwrap();
            assert_eq!(health.status(), StatusCode::OK);
            let readyz = client
                .get(server.http_url("/demo/readyz"))
                .send()
                .await
                .unwrap();
            assert_eq!(readyz.status(), StatusCode::OK);

            let initialized = initialize_http(&client, &endpoint).await;
            assert_eq!(initialized.status(), StatusCode::OK);
            let connection_id = initialized
                .headers()
                .get(CONNECTION_ID)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            let initialized: Value =
                serde_json::from_str(&initialized.text().await.unwrap()).unwrap();
            assert_eq!(initialized["result"]["protocolVersion"], json!(1));

            let sse = client
                .get(&endpoint)
                .header(ACCEPT, "text/event-stream")
                .header(CONNECTION_ID, &connection_id)
                .send()
                .await
                .unwrap();
            assert_eq!(sse.status(), StatusCode::OK);
            let mut events = sse.bytes_stream();
            let accepted = client
                .post(&endpoint)
                .header(CONTENT_TYPE, "application/json")
                .header(CONNECTION_ID, &connection_id)
                .body(ECHO_REQUEST)
                .send()
                .await
                .unwrap();
            assert_eq!(accepted.status(), StatusCode::ACCEPTED);
            let event = timeout(Duration::from_secs(5), events.next())
                .await
                .expect("SSE response timed out")
                .unwrap()
                .unwrap();
            assert!(
                std::str::from_utf8(&event)
                    .unwrap()
                    .contains(r#""echo":"ok""#)
            );
            drop(events);

            let deleted = client
                .delete(&endpoint)
                .header(CONNECTION_ID, &connection_id)
                .send()
                .await
                .unwrap();
            assert_eq!(deleted.status(), StatusCode::ACCEPTED);
            let unregistered = client
                .delete(server.http_url("/api/agents"))
                .json(&json!({ "id": "demo" }))
                .send()
                .await
                .unwrap();
            assert_eq!(unregistered.status(), StatusCode::NO_CONTENT);
            assert_eq!(
                client
                    .get(server.http_url("/demo/health"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::NOT_FOUND
            );
            server.stop().await;
        }

        #[tokio::test]
        async fn unregister_keeps_existing_websocket_and_rejects_new_requests() {
            let server = NamedAcpServer::start(Duration::from_millis(50)).await;
            let (mut socket, response) = connect_async(server.ws_url("/demo/acp")).await.unwrap();
            assert!(response.headers().contains_key(CONNECTION_ID));
            initialize_websocket(&mut socket).await;

            let removed = reqwest::Client::new()
                .delete(server.http_url("/api/agents"))
                .json(&json!({ "id": "demo" }))
                .send()
                .await
                .unwrap();
            assert_eq!(removed.status(), StatusCode::NO_CONTENT);
            assert_eq!(
                reqwest::get(server.http_url("/demo/health"))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::NOT_FOUND
            );

            socket
                .send(Message::Text(ECHO_REQUEST.into()))
                .await
                .unwrap();
            let frame = timeout(Duration::from_secs(5), socket.next())
                .await
                .expect("WebSocket echo timed out")
                .unwrap()
                .unwrap();
            let Message::Text(text) = frame else {
                panic!("expected text echo response, got {frame:?}");
            };
            let response: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(response["result"]["echo"], "ok");
            socket.close(None).await.unwrap();
            server.stop().await;
        }

        #[tokio::test]
        async fn named_readyz_reports_agent_spawn_failure() {
            let missing = format!("/definitely-missing-named-agent-{}", std::process::id());
            let server = NamedAcpServer::start_with_agent(
                AcpAgentConfig::new(missing),
                Duration::from_millis(50),
            )
            .await;
            let client = reqwest::Client::new();
            let response = initialize_http(&client, &server.http_url("/demo/acp")).await;
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

            let readyz = timeout(Duration::from_secs(5), async {
                loop {
                    let response = client
                        .get(server.http_url("/demo/readyz"))
                        .send()
                        .await
                        .unwrap();
                    if response.status() == StatusCode::SERVICE_UNAVAILABLE {
                        break response.text().await.unwrap();
                    }
                    sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("named readyz did not report the launch failure");
            assert!(readyz.contains("agent launches failed"));
            server.stop().await;
        }

        #[tokio::test]
        async fn shutdown_bounds_active_sse_and_websocket_connections() {
            let mut server = NamedAcpServer::start(Duration::from_millis(50)).await;
            let client = reqwest::Client::new();
            let endpoint = server.http_url("/demo/acp");
            let initialized = initialize_http(&client, &endpoint).await;
            let connection_id = initialized
                .headers()
                .get(CONNECTION_ID)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            let sse = client
                .get(&endpoint)
                .header(ACCEPT, "text/event-stream")
                .header(CONNECTION_ID, connection_id)
                .send()
                .await
                .unwrap();
            let mut events = sse.bytes_stream();
            let (mut socket, _) = connect_async(server.ws_url("/demo/acp")).await.unwrap();
            initialize_websocket(&mut socket).await;

            let shutdown_at = tokio::time::Instant::now();
            let response = client
                .post(server.http_url("/api/shutdown"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            server.wait().await;
            assert!(shutdown_at.elapsed() < Duration::from_secs(1));

            let sse_end = timeout(Duration::from_secs(1), events.next())
                .await
                .expect("SSE connection remained open after shutdown");
            assert!(
                sse_end.is_none() || sse_end.as_ref().is_some_and(|result| result.is_err()),
                "SSE produced data instead of closing after shutdown"
            );
            let websocket_end = timeout(Duration::from_secs(1), socket.next())
                .await
                .expect("WebSocket connection remained open after shutdown");
            assert!(
                matches!(
                    &websocket_end,
                    None | Some(Err(_)) | Some(Ok(Message::Close(_)))
                ),
                "WebSocket produced a non-close frame after shutdown: {websocket_end:?}"
            );
        }
    }
}
