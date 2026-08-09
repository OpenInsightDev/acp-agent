//! Named ACP servers with dynamically registered in-process agent routers.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
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
    /// Whether to inject the agent's yolo argument.
    pub yolo: bool,
    /// Arguments forwarded to the agent process on connection.
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ServerFile {
    name: String,
    host: String,
    port: u16,
    pid: u32,
}

/// Wire DTO for `POST /api/agents`; do not expose `ServeOptions` directly.
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
    #[serde(default)]
    yolo: bool,
    #[serde(default)]
    args: Vec<String>,
}

fn default_true() -> bool {
    true
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

#[derive(Clone)]
struct ServerState {
    agents: Arc<RwLock<HashMap<String, RegisteredAgent>>>,
    shutdown: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

#[derive(Clone)]
struct RegisteredAgent {
    id: String,
    route: String,
    router: Router,
}

/// Starts a named ACP server in the background and returns its URL.
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

/// Stops a named ACP server.
pub async fn stop(name: &str) -> Result<String> {
    validate_name(name)?;
    let path = server_file(name)?;
    let state: ServerFile = read_json(&path)
        .await
        .with_context(|| format!("server \"{name}\" is not running"))?;
    let response = reqwest::Client::new()
        .post(format!("{}/api/shutdown", control_url(&state)))
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

/// Registers an in-process agent router with a named ACP server.
pub async fn register(agent_id: &str, options: RegisterOptions) -> Result<String> {
    validate_name(&options.name)?;
    let route = options.route.unwrap_or_else(|| format!("/{agent_id}"));
    validate_route(&route)?;
    let state = load_live_server(&options.name).await?;
    let registration = AgentRegistrationRequest {
        id: agent_id.to_string(),
        route: route.clone(),
        serve: AgentServeRequest {
            path: options.path,
            cors_origins: options.cors_origins,
            allow_any_origin: options.allow_any_origin,
            health_endpoint: options.health_endpoint,
            readyz_endpoint: options.readyz_endpoint,
            yolo: options.yolo,
            args: options.args,
        },
    };
    let response = reqwest::Client::new()
        .post(format!("{}/api/agents", control_url(&state)))
        .json(&registration)
        .send()
        .await
        .with_context(|| format!("failed to register with server \"{}\"", options.name))?;
    if !response.status().is_success() {
        let detail = response.text().await.unwrap_or_default();
        bail!(
            "server rejected agent registration: {}",
            api_error_detail(&detail)
        );
    }
    Ok(format!(
        "registered agent \"{agent_id}\" at {}{route}",
        public_url(&state.host, state.port)
    ))
}

/// Removes an agent router from a named ACP server.
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
        bail!(
            "server rejected agent unregistration: {}",
            api_error_detail(&detail)
        );
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
        .with_context(|| format!("failed to bind named server on {host}:{port}"))?;
    let address = listener
        .local_addr()
        .context("failed to read named server address")?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let state = ServerState {
        agents: Arc::default(),
        shutdown: Arc::new(Mutex::new(Some(shutdown_tx))),
    };
    let router = server_router(state.clone());
    let file = ServerFile {
        name: name.clone(),
        host,
        port: address.port(),
        pid: std::process::id(),
    };
    write_json(&server_file(&name)?, &file).await?;
    eprintln!(
        "Serving named ACP server \"{name}\" at {}",
        public_url(&file.host, file.port)
    );

    let result = axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        })
        .await
        .context("named ACP server failed");
    let _ = tokio::fs::remove_file(server_file(&name)?).await;
    result
}

fn server_router(state: ServerState) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/status", get(server_status))
        .route("/api/agents", post(add_agent).delete(remove_agent))
        .route("/api/shutdown", post(shutdown))
        .fallback(dispatch_agent)
        .with_state(state)
}

async fn server_status() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

async fn add_agent(
    State(state): State<ServerState>,
    registration: std::result::Result<Json<AgentRegistrationRequest>, JsonRejection>,
) -> Response {
    let Json(registration) = match registration {
        Ok(registration) => registration,
        Err(error) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                error.body_text(),
            );
        }
    };
    if let Err(error) = validate_route(&registration.route) {
        return api_error(StatusCode::BAD_REQUEST, "invalid_route", error.to_string());
    }

    let options = match serve_options(&registration.serve) {
        Ok(options) => options,
        Err(error) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_options",
                error.to_string(),
            );
        }
    };
    let registry = match crate::registry::fetch_registry().await {
        Ok(registry) => registry,
        Err(error) => {
            return api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "registry_unavailable",
                error.to_string(),
            );
        }
    };
    let Some(agent) = registry.find_agent(&registration.id) else {
        return api_error(
            StatusCode::NOT_FOUND,
            "agent_not_found",
            format!("agent {} was not found in the registry", registration.id),
        );
    };
    let args = match resolved_args(&registration.id, &registration.serve).await {
        Ok(args) => args,
        Err(error) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_options",
                error.to_string(),
            );
        }
    };
    let config = match crate::runner::resolve_agent_config_from_registry_agent(agent, &args).await {
        Ok(config) => config,
        Err(error) => {
            return api_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "agent_unavailable",
                error.to_string(),
            );
        }
    };
    // Router construction may validate configuration and must happen before
    // taking the registry lock; fetching a registry or resolving a binary can
    // be slow and must not block existing dispatches or registrations.
    let router = match crate::commands::serve::agent_router(config, &options) {
        Ok(router) => router,
        Err(error) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_options",
                error.to_string(),
            );
        }
    };
    let entry = RegisteredAgent {
        id: registration.id,
        route: registration.route,
        router,
    };

    match insert_agent(&state, entry).await {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(error) => error,
    }
}

fn api_error_detail(body: &str) -> String {
    match serde_json::from_str::<ApiError>(body) {
        Ok(error) => format!("{}: {}", error.error, error.message),
        Err(_) if body.trim().is_empty() => "server returned no error details".to_string(),
        Err(_) => body.to_string(),
    }
}

fn serve_options(request: &AgentServeRequest) -> Result<crate::commands::serve::ServeOptions> {
    let options = crate::commands::serve::ServeOptions {
        host: "127.0.0.1".to_string(),
        port: 0,
        subpath: None,
        path: request.path.clone(),
        cors: crate::commands::serve::cors_options(
            request.cors_origins.clone(),
            request.allow_any_origin,
        )?,
        health_endpoint: request.health_endpoint,
        readyz_endpoint: request.readyz_endpoint,
    };
    crate::commands::serve::validate_options(&options)?;
    Ok(options)
}

async fn resolved_args(id: &str, request: &AgentServeRequest) -> Result<Vec<String>> {
    if !request.yolo {
        return Ok(request.args.clone());
    }
    let extra = crate::yolo::yolo_extra_args(id).await?;
    Ok(extra.into_iter().chain(request.args.clone()).collect())
}

async fn insert_agent(
    state: &ServerState,
    entry: RegisteredAgent,
) -> std::result::Result<(), Response> {
    let mut agents = state.agents.write().await;
    if agents.contains_key(&entry.id) {
        return Err(api_error(
            StatusCode::CONFLICT,
            "agent_id_conflict",
            format!("agent id {} is already registered", entry.id),
        ));
    }
    if agents
        .values()
        .any(|existing| existing.route == entry.route)
    {
        return Err(api_error(
            StatusCode::CONFLICT,
            "route_conflict",
            format!("route {} is already registered", entry.route),
        ));
    }
    agents.insert(entry.id.clone(), entry);
    Ok(())
}

async fn remove_agent(
    State(state): State<ServerState>,
    selector: std::result::Result<Json<AgentSelector>, JsonRejection>,
) -> Response {
    let Json(selector) = match selector {
        Ok(selector) => selector,
        Err(error) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                error.body_text(),
            );
        }
    };
    let mut agents = state.agents.write().await;
    if agents.remove(&selector.id).is_none() {
        return api_error(
            StatusCode::NOT_FOUND,
            "agent_not_found",
            format!("agent {} is not registered", selector.id),
        );
    }
    // Connections that have already cloned the router continue naturally;
    // removing this entry prevents only new requests from being dispatched.
    StatusCode::NO_CONTENT.into_response()
}

async fn shutdown(State(state): State<ServerState>) -> Response {
    if let Some(sender) = state.shutdown.lock().await.take() {
        let _ = sender.send(());
    }
    StatusCode::ACCEPTED.into_response()
}

async fn dispatch_agent(State(state): State<ServerState>, request: Request<Body>) -> Response {
    let path = request.uri().path();
    let entry = {
        let agents = state.agents.read().await;
        agents
            .values()
            .filter(|entry| route_matches(&entry.route, path))
            .max_by_key(|entry| entry.route.len())
            .cloned()
    };
    let Some(entry) = entry else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let request = match rewrite_route_prefix(request, &entry.route) {
        Ok(request) => request,
        Err(error) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "uri_rewrite_failed",
                error,
            );
        }
    };
    // The read lock above has been released. This is necessary because a
    // router can own an SSE or WebSocket connection for an unbounded period.
    match entry.router.oneshot(request).await {
        Ok(response) => response.into_response(),
        Err(error) => match error {},
    }
}

fn rewrite_route_prefix(
    mut request: Request<Body>,
    route: &str,
) -> std::result::Result<Request<Body>, String> {
    let path = request.uri().path();
    let suffix = path
        .strip_prefix(route)
        .filter(|suffix| suffix.is_empty() || suffix.starts_with('/'))
        .ok_or_else(|| format!("route {route} does not match request path {path}"))?;
    let rewritten_path = if suffix.is_empty() { "/" } else { suffix };
    let path_and_query = match request.uri().query() {
        Some(query) => format!("{rewritten_path}?{query}"),
        None => rewritten_path.to_string(),
    };
    let uri = Uri::builder()
        .path_and_query(path_and_query)
        .build()
        .map_err(|error| format!("failed to rewrite request URI: {error}"))?;
    *request.uri_mut() = uri;
    Ok(request)
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
    let uri: Uri = route
        .parse()
        .context("agent route is not a valid URI path")?;
    if uri.path() != route || uri.query().is_some() {
        bail!("agent route is not a valid URI path");
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

fn server_log_file(name: &str) -> Result<PathBuf> {
    Ok(cache_directory()?.join(format!("{name}.log")))
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
        other if other.contains(':') && !other.starts_with('[') => {
            return format!("http://[{other}]:{}", state.port);
        }
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

#[cfg(test)]
mod tests {
    use axum::routing::any;
    use serde_json::Value;
    use std::net::SocketAddr;

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
        let state = ServerFile {
            name: "test".into(),
            host: "0.0.0.0".into(),
            port: 8010,
            pid: 1,
        };
        assert_eq!(control_url(&state), "http://127.0.0.1:8010");
        let state = ServerFile {
            host: "::1".into(),
            ..state
        };
        assert_eq!(control_url(&state), "http://[::1]:8010");
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

    fn test_state() -> ServerState {
        let (shutdown, _) = oneshot::channel();
        ServerState {
            agents: Arc::default(),
            shutdown: Arc::new(Mutex::new(Some(shutdown))),
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
    async fn dispatches_the_longest_matching_router_and_preserves_the_suffix() {
        let state = test_state();
        insert_agent(
            &state,
            RegisteredAgent {
                id: "root".into(),
                route: "/agent".into(),
                router: echo_router(),
            },
        )
        .await
        .unwrap();
        insert_agent(
            &state,
            RegisteredAgent {
                id: "nested".into(),
                route: "/agent/child".into(),
                router: Router::new().fallback(any(|| async { "nested" })),
            },
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
    async fn rejects_duplicate_agent_ids_and_routes() {
        let state = test_state();
        insert_agent(
            &state,
            RegisteredAgent {
                id: "demo".into(),
                route: "/demo".into(),
                router: echo_router(),
            },
        )
        .await
        .unwrap();
        let duplicate_id = insert_agent(
            &state,
            RegisteredAgent {
                id: "demo".into(),
                route: "/other".into(),
                router: echo_router(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(duplicate_id.status(), StatusCode::CONFLICT);
        let duplicate_route = insert_agent(
            &state,
            RegisteredAgent {
                id: "other".into(),
                route: "/demo".into(),
                router: echo_router(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(duplicate_route.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn management_api_is_unauthenticated_and_returns_json_errors() {
        let (address, task) = spawn_axum(server_router(test_state())).await;
        let client = reqwest::Client::new();
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
}
