//! Named reverse-proxy servers and their locally managed agent processes.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use axum_reverse_proxy::ReverseProxy;
use serde::{Deserialize, Serialize};
use tokio::{
    net::TcpListener,
    process::Command,
    sync::{Mutex, RwLock, oneshot},
    time::{sleep, timeout},
};
use tower::ServiceExt;

const DEFAULT_NAME: &str = "default";
const START_TIMEOUT: Duration = Duration::from_secs(10);
const AGENT_START_TIMEOUT: Duration = Duration::from_secs(60);

/// Options accepted by `server start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartOptions {
    /// Stable local name used by subsequent server commands.
    pub name: String,
    /// Address on which the reverse proxy listens.
    pub host: String,
    /// Port on which the reverse proxy listens.
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
    /// ACP endpoint path on the private agent server.
    pub path: String,
    /// Browser origins allowed by the private agent server.
    pub cors_origins: Vec<String>,
    /// Whether all browser origins are accepted.
    pub allow_any_origin: bool,
    /// Whether the private agent server exposes `/health`.
    pub health_endpoint: bool,
    /// Whether the private agent server exposes `/readyz`.
    pub readyz_endpoint: bool,
    /// Whether to inject the agent's yolo argument.
    pub yolo: bool,
    /// Arguments forwarded to the agent process.
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ServerFile {
    name: String,
    host: String,
    port: u16,
    pid: u32,
    shutdown_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentRegistration {
    id: String,
    route: String,
    target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
struct AgentSelector {
    id: String,
}

#[derive(Clone)]
struct ProxyState {
    server_name: String,
    agents: Arc<RwLock<HashMap<String, ProxyEntry>>>,
    shutdown: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    shutdown_token: String,
}

#[derive(Clone)]
struct ProxyEntry {
    registration: AgentRegistration,
    router: Router,
}

impl ProxyEntry {
    fn new(registration: AgentRegistration) -> Self {
        let proxy = ReverseProxy::new(registration.route.clone(), registration.target.clone());
        Self {
            registration,
            router: Router::new().fallback_service(proxy),
        }
    }
}

/// Starts a named reverse proxy in the background and returns its URL.
pub async fn start(options: StartOptions) -> Result<String> {
    validate_name(&options.name)?;
    let path = server_file(&options.name)?;
    if let Ok(existing) = read_json::<ServerFile>(&path).await {
        if server_is_alive(&existing).await {
            bail!(
                "server \"{}\" is already running at {}",
                options.name,
                public_url(&existing.host, existing.port)
            );
        }
        let _ = tokio::fs::remove_file(&path).await;
    }
    clean_stale_agents(&options.name).await?;

    let executable = std::env::current_exe().context("failed to locate acp-agent executable")?;
    let log_path = server_log_file(&options.name)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open server log {}", log_path.display()))?;
    let stderr = log
        .try_clone()
        .context("failed to clone server log handle")?;
    let mut command = Command::new(executable);
    command
        .arg("__server-run")
        .arg("--name")
        .arg(&options.name)
        .arg("--host")
        .arg(&options.host)
        .arg("--port")
        .arg(options.port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    detach_process(&mut command);
    let mut child = command.spawn().context("failed to start server process")?;

    let ready = async {
        loop {
            if let Some(status) = child
                .try_wait()
                .context("failed to inspect server process")?
            {
                bail!(
                    "server process exited with {status}; inspect {}",
                    log_path.display()
                );
            }
            if let Ok(state) = read_json::<ServerFile>(&path).await
                && server_is_alive(&state).await
            {
                return Ok(state);
            }
            sleep(Duration::from_millis(50)).await;
        }
    };
    let state = timeout(START_TIMEOUT, ready)
        .await
        .context("timed out waiting for server to start")??;
    Ok(format!(
        "started server \"{}\" at {}",
        state.name,
        public_url(&state.host, state.port)
    ))
}

/// Stops a named reverse proxy and its locally managed agent servers.
pub async fn stop(name: &str) -> Result<String> {
    validate_name(name)?;
    let path = server_file(name)?;
    let state: ServerFile = read_json(&path)
        .await
        .with_context(|| format!("server \"{name}\" is not running"))?;
    let response = reqwest::Client::new()
        .post(format!("{}/api/shutdown", control_url(&state)))
        .header("x-acp-agent-token", &state.shutdown_token)
        .send()
        .await
        .with_context(|| format!("failed to contact server \"{name}\""))?;
    if !response.status().is_success() {
        bail!("server \"{name}\" rejected the shutdown request");
    }

    let wait = async {
        while tokio::fs::try_exists(&path).await.unwrap_or(false) {
            sleep(Duration::from_millis(50)).await;
        }
    };
    timeout(START_TIMEOUT, wait)
        .await
        .context("timed out waiting for server to stop")?;
    Ok(format!("stopped server \"{name}\""))
}

/// Starts an agent HTTP server and registers it with a named reverse proxy.
pub async fn register(agent_id: &str, options: RegisterOptions) -> Result<String> {
    validate_name(&options.name)?;
    let route = options.route.unwrap_or_else(|| format!("/{agent_id}"));
    validate_route(&route)?;
    let state = load_live_server(&options.name).await?;
    let port = reserve_loopback_port()?;
    let executable = std::env::current_exe().context("failed to locate acp-agent executable")?;
    let log_path = agent_log_file(&options.name, agent_id)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open agent log {}", log_path.display()))?;
    let stderr = log
        .try_clone()
        .context("failed to clone agent log handle")?;
    let mut command = Command::new(executable);
    command
        .arg("serve")
        .arg(agent_id)
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--path")
        .arg(&options.path);
    for origin in &options.cors_origins {
        command.arg("--cors-origin").arg(origin);
    }
    if options.allow_any_origin {
        command.arg("--allow-any-origin");
    }
    if !options.health_endpoint {
        command.arg("--no-health");
    }
    if !options.readyz_endpoint {
        command.arg("--no-readyz");
    }
    if options.yolo {
        command.arg("--yolo");
    }
    if !options.args.is_empty() {
        command.arg("--").args(&options.args);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    detach_process(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start agent \"{agent_id}\""))?;
    let target = format!("http://127.0.0.1:{port}");

    let wait_for_agent = async {
        loop {
            if let Some(status) = child
                .try_wait()
                .context("failed to inspect agent process")?
            {
                bail!(
                    "agent server exited with {status}; inspect {}",
                    log_path.display()
                );
            }
            if tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                .await
                .is_ok()
            {
                return Result::<()>::Ok(());
            }
            sleep(Duration::from_millis(100)).await;
        }
    };
    match timeout(AGENT_START_TIMEOUT, wait_for_agent).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let _ = child.kill().await;
            return Err(error);
        }
        Err(error) => {
            let _ = child.kill().await;
            return Err(error).context("timed out waiting for agent server to start");
        }
    }
    let registration = AgentRegistration {
        id: agent_id.to_string(),
        route: route.clone(),
        target,
        pid: child.id(),
    };
    let response = reqwest::Client::new()
        .post(format!("{}/api/agents", control_url(&state)))
        .header("x-acp-agent-token", &state.shutdown_token)
        .json(&registration)
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            let _ = child.kill().await;
            return Err(error)
                .with_context(|| format!("failed to register with server \"{}\"", options.name));
        }
    };
    if !response.status().is_success() {
        let detail = response.text().await.unwrap_or_default();
        let _ = child.kill().await;
        bail!("server rejected agent registration: {detail}");
    }
    Ok(format!(
        "registered agent \"{agent_id}\" at {}{route}",
        public_url(&state.host, state.port)
    ))
}

/// Unregisters an agent and stops its locally managed HTTP server.
pub async fn unregister(agent_id: &str, name: &str) -> Result<String> {
    validate_name(name)?;
    let state = load_live_server(name).await?;
    let response = reqwest::Client::new()
        .delete(format!("{}/api/agents", control_url(&state)))
        .json(&AgentSelector {
            id: agent_id.to_string(),
        })
        .send()
        .await
        .with_context(|| format!("failed to contact server \"{name}\""))?;
    if !response.status().is_success() {
        let detail = response.text().await.unwrap_or_default();
        bail!("server rejected agent unregistration: {detail}");
    }
    Ok(format!(
        "unregistered agent \"{agent_id}\" from server \"{name}\""
    ))
}

/// Runs the foreground process behind the hidden `__server-run` command.
pub async fn run(name: String, host: String, port: u16) -> Result<()> {
    validate_name(&name)?;
    let listener = TcpListener::bind((host.as_str(), port))
        .await
        .with_context(|| format!("failed to bind reverse proxy on {host}:{port}"))?;
    let address = listener
        .local_addr()
        .context("failed to read reverse proxy address")?;
    let token = shutdown_token();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let state = ProxyState {
        server_name: name.clone(),
        agents: Arc::default(),
        shutdown: Arc::new(Mutex::new(Some(shutdown_tx))),
        shutdown_token: token.clone(),
    };
    let router = proxy_router(state.clone());
    let file = ServerFile {
        name: name.clone(),
        host,
        port: address.port(),
        pid: std::process::id(),
        shutdown_token: token,
    };
    write_json(&server_file(&name)?, &file).await?;
    eprintln!(
        "Serving reverse proxy \"{name}\" at {}",
        public_url(&file.host, file.port)
    );

    let result = axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        })
        .await
        .context("reverse proxy server failed");
    cleanup_registered_agents(&state).await;
    let _ = tokio::fs::remove_file(server_file(&name)?).await;
    let _ = tokio::fs::remove_file(agents_file(&name)?).await;
    result
}

fn proxy_router(state: ProxyState) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/status", get(server_status))
        .route("/api/agents", post(add_agent).delete(remove_agent))
        .route("/api/shutdown", post(shutdown))
        .fallback(proxy)
        .with_state(state)
}

async fn server_status(State(state): State<ProxyState>, request: Request<Body>) -> Response {
    let supplied = request
        .headers()
        .get("x-acp-agent-token")
        .and_then(|value| value.to_str().ok());
    if supplied != Some(&state.shutdown_token) {
        return StatusCode::FORBIDDEN.into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn add_agent(
    State(state): State<ProxyState>,
    headers: axum::http::HeaderMap,
    Json(registration): Json<AgentRegistration>,
) -> Response {
    let mut registration = registration;
    if headers
        .get("x-acp-agent-token")
        .and_then(|value| value.to_str().ok())
        != Some(&state.shutdown_token)
    {
        // The public API may point at any upstream, but only this CLI may ask
        // the daemon to manage a local process lifecycle.
        registration.pid = None;
    }
    if let Err(error) = validate_route(&registration.route) {
        return (StatusCode::BAD_REQUEST, error.to_string()).into_response();
    }
    if registration
        .target
        .parse::<Uri>()
        .ok()
        .is_none_or(|uri| uri.scheme_str() != Some("http") || uri.authority().is_none())
    {
        return (
            StatusCode::BAD_REQUEST,
            "target must be an absolute http URL",
        )
            .into_response();
    }
    let mut agents = state.agents.write().await;
    if agents.contains_key(&registration.id) {
        return (StatusCode::CONFLICT, "agent id is already registered").into_response();
    }
    if agents
        .values()
        .any(|existing| existing.registration.route == registration.route)
    {
        return (StatusCode::CONFLICT, "route is already registered").into_response();
    }
    let registration_id = registration.id.clone();
    agents.insert(registration_id.clone(), ProxyEntry::new(registration));
    if let Err(error) = persist_agents(&state.server_name, &agents).await {
        agents.remove(&registration_id);
        return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
    }
    StatusCode::CREATED.into_response()
}

async fn remove_agent(
    State(state): State<ProxyState>,
    Json(selector): Json<AgentSelector>,
) -> Response {
    let registration = {
        let mut agents = state.agents.write().await;
        let Some(entry) = agents.remove(&selector.id) else {
            return (StatusCode::NOT_FOUND, "agent is not registered").into_response();
        };
        if let Err(error) = persist_agents(&state.server_name, &agents).await {
            agents.insert(entry.registration.id.clone(), entry);
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
        entry.registration
    };
    if let Some(pid) = registration.pid {
        terminate_process(pid).await;
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn shutdown(State(state): State<ProxyState>, request: Request<Body>) -> Response {
    let supplied = request
        .headers()
        .get("x-acp-agent-token")
        .and_then(|value| value.to_str().ok());
    if supplied != Some(&state.shutdown_token) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Some(sender) = state.shutdown.lock().await.take() {
        let _ = sender.send(());
    }
    StatusCode::ACCEPTED.into_response()
}

async fn proxy(State(state): State<ProxyState>, request: Request<Body>) -> Response {
    let path = request.uri().path();
    let entry = {
        let agents = state.agents.read().await;
        agents
            .values()
            .filter(|entry| route_matches(&entry.registration.route, path))
            .max_by_key(|entry| entry.registration.route.len())
            .cloned()
    };
    let Some(entry) = entry else {
        return StatusCode::NOT_FOUND.into_response();
    };
    entry.router.oneshot(request).await.unwrap()
}

async fn cleanup_registered_agents(state: &ProxyState) {
    let entries: Vec<_> = state.agents.read().await.values().cloned().collect();
    for entry in entries {
        if let Some(pid) = entry.registration.pid {
            terminate_process(pid).await;
        }
    }
}

async fn clean_stale_agents(name: &str) -> Result<()> {
    let path = agents_file(name)?;
    // A stale PID may have been reused by the operating system. Only a live
    // server may terminate processes from its in-memory registration table.
    let _ = tokio::fs::remove_file(path).await;
    Ok(())
}

async fn persist_agents(name: &str, agents: &HashMap<String, ProxyEntry>) -> Result<()> {
    let registrations: Vec<_> = agents
        .values()
        .map(|entry| entry.registration.clone())
        .collect();
    write_json(&agents_file(name)?, &registrations).await
}

async fn load_live_server(name: &str) -> Result<ServerFile> {
    let state: ServerFile = read_json(&server_file(name)?)
        .await
        .with_context(|| format!("server \"{name}\" is not running"))?;
    if !server_is_alive(&state).await {
        bail!("server \"{name}\" is not running");
    }
    Ok(state)
}

async fn server_is_alive(state: &ServerFile) -> bool {
    reqwest::Client::new()
        .get(format!("{}/api/status", control_url(state)))
        .header("x-acp-agent-token", &state.shutdown_token)
        .timeout(Duration::from_millis(500))
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

fn route_matches(route: &str, path: &str) -> bool {
    path == route
        || path
            .strip_prefix(route)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn reserve_loopback_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .context("failed to reserve an agent server port")?;
    Ok(listener.local_addr()?.port())
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("server name must contain only letters, digits, '.', '-' or '_'");
    }
    Ok(())
}

fn validate_route(route: &str) -> Result<()> {
    if !route.starts_with('/') || route == "/" || route.ends_with('/') {
        bail!("agent route must start with '/', cannot be '/', and must not end with '/'");
    }
    if route.contains(['?', '#']) || route.split('/').any(|part| part == "..") {
        bail!("agent route contains an invalid path segment");
    }
    if route == "/api" || route.starts_with("/api/") || route == "/health" {
        bail!("agent route conflicts with a server endpoint");
    }
    Ok(())
}

fn cache_directory() -> Result<PathBuf> {
    let base = dirs::cache_dir().context("failed to locate the platform cache directory")?;
    let path = base.join("acp-agent").join("servers");
    std::fs::create_dir_all(&path)
        .with_context(|| format!("failed to create server state directory {}", path.display()))?;
    Ok(path)
}

fn server_file(name: &str) -> Result<PathBuf> {
    Ok(cache_directory()?.join(format!("{name}.json")))
}

fn agents_file(name: &str) -> Result<PathBuf> {
    Ok(cache_directory()?.join(format!("{name}.agents.json")))
}

fn server_log_file(name: &str) -> Result<PathBuf> {
    Ok(cache_directory()?.join(format!("{name}.log")))
}

fn agent_log_file(name: &str, agent_id: &str) -> Result<PathBuf> {
    let safe_id: String = agent_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    Ok(cache_directory()?.join(format!("{name}.{safe_id}.log")))
}

async fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("failed to parse {}", path.display()))
}

async fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).context("failed to serialize server state")?;
    tokio::fs::write(path, bytes)
        .await
        .with_context(|| format!("failed to write {}", path.display()))
}

fn control_url(state: &ServerFile) -> String {
    let host = match state.host.as_str() {
        "0.0.0.0" => "127.0.0.1",
        "::" | "[::]" => "[::1]",
        other => other,
    };
    format!("http://{host}:{}", state.port)
}

fn public_url(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("http://[{host}]:{port}")
    } else {
        format!("http://{host}:{port}")
    }
}

fn shutdown_token() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(unix)]
fn detach_process(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.as_std_mut().process_group(0);
}

#[cfg(windows)]
fn detach_process(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    command
        .as_std_mut()
        .creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
}

#[cfg(unix)]
async fn terminate_process(pid: u32) {
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .await;
}

#[cfg(windows)]
async fn terminate_process(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status()
        .await;
}

#[cfg(test)]
mod tests {
    use async_tungstenite::tungstenite::Message;
    use futures::StreamExt;

    use super::*;

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
    }

    #[test]
    fn matches_only_complete_route_segments() {
        assert!(route_matches("/agent", "/agent"));
        assert!(route_matches("/agent", "/agent/acp"));
        assert!(!route_matches("/agent", "/agent-two/acp"));
    }

    #[test]
    fn formats_control_urls_for_wildcard_listeners() {
        let state = ServerFile {
            name: "test".into(),
            host: "0.0.0.0".into(),
            port: 8010,
            pid: 1,
            shutdown_token: "token".into(),
        };
        assert_eq!(control_url(&state), "http://127.0.0.1:8010");
    }

    fn test_state(name: String) -> ProxyState {
        let (shutdown, _) = oneshot::channel();
        ProxyState {
            server_name: name,
            agents: Arc::default(),
            shutdown: Arc::new(Mutex::new(Some(shutdown))),
            shutdown_token: "test-token".to_string(),
        }
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
    async fn dynamically_registers_proxies_and_unregisters_http_routes() {
        let upstream = Router::new().fallback(|request: Request<Body>| async move {
            request
                .uri()
                .path_and_query()
                .map_or("/", |value| value.as_str())
                .to_string()
        });
        let (upstream_address, upstream_task) = spawn_axum(upstream).await;
        let name = format!("test-{}", uuid::Uuid::new_v4());
        let (proxy_address, proxy_task) = spawn_axum(proxy_router(test_state(name.clone()))).await;
        let client = reqwest::Client::new();
        let registration = AgentRegistration {
            id: "demo".to_string(),
            route: "/demo".to_string(),
            target: format!("http://{upstream_address}"),
            pid: None,
        };

        let added = client
            .post(format!("http://{proxy_address}/api/agents"))
            .json(&registration)
            .send()
            .await
            .unwrap();
        assert_eq!(added.status(), StatusCode::CREATED);

        let proxied = client
            .get(format!("http://{proxy_address}/demo/acp?mode=test"))
            .send()
            .await
            .unwrap();
        assert_eq!(proxied.status(), StatusCode::OK);
        assert_eq!(proxied.text().await.unwrap(), "/acp?mode=test");

        let removed = client
            .delete(format!("http://{proxy_address}/api/agents"))
            .json(&AgentSelector { id: "demo".into() })
            .send()
            .await
            .unwrap();
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);
        let missing = client
            .get(format!("http://{proxy_address}/demo/acp"))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        proxy_task.abort();
        upstream_task.abort();
        let _ = tokio::fs::remove_file(agents_file(&name).unwrap()).await;
    }

    #[tokio::test]
    async fn proxies_websocket_connections() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let (stream, _) = upstream.accept().await.unwrap();
            let mut websocket = async_tungstenite::tokio::accept_async(stream)
                .await
                .unwrap();
            if let Some(Ok(message)) = websocket.next().await {
                websocket.send(message).await.unwrap();
            }
        });
        let name = format!("test-{}", uuid::Uuid::new_v4());
        let state = test_state(name.clone());
        state.agents.write().await.insert(
            "demo".into(),
            ProxyEntry::new(AgentRegistration {
                id: "demo".into(),
                route: "/demo".into(),
                target: format!("http://{upstream_address}"),
                pid: None,
            }),
        );
        let (proxy_address, proxy_task) = spawn_axum(proxy_router(state)).await;

        let (mut websocket, _) =
            async_tungstenite::tokio::connect_async(format!("ws://{proxy_address}/demo/acp"))
                .await
                .unwrap();
        websocket.send(Message::Text("hello".into())).await.unwrap();
        assert_eq!(
            websocket.next().await.unwrap().unwrap(),
            Message::Text("hello".into())
        );

        proxy_task.abort();
        upstream_task.abort();
        let _ = tokio::fs::remove_file(agents_file(&name).unwrap()).await;
    }
}
