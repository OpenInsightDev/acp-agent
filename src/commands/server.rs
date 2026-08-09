//! Named ACP servers with dynamically registered in-process agent routers.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::future::IntoFuture;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
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
use serde::{Deserialize, Serialize};
use tokio::{
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
const SERVER_STATE_DIR_ENV: &str = "ACP_AGENT_SERVER_STATE_DIR";
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    listen_host: String,
    port: u16,
    control_url: String,
    pid: u32,
    version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ServerStatus {
    name: String,
    pid: u32,
    version: String,
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
    server_name: String,
    agents: Arc<RwLock<HashMap<String, RegisteredAgent>>>,
    shutdown: watch::Sender<bool>,
}

#[derive(Clone)]
struct RegisteredAgent {
    id: String,
    route: String,
    router: Router,
}

#[derive(Debug, Clone)]
struct ServerPaths {
    directory: PathBuf,
}

impl ServerPaths {
    fn discover() -> Result<Self> {
        if let Some(directory) = std::env::var_os(SERVER_STATE_DIR_ENV) {
            return Self::new(PathBuf::from(directory));
        }
        let base = dirs::cache_dir().context("failed to locate the platform cache directory")?;
        Self::new(base.join("acp-agent").join("servers"))
    }

    fn new(directory: PathBuf) -> Result<Self> {
        ensure_private_directory(&directory)?;
        Ok(Self { directory })
    }

    fn state_file(&self, name: &str) -> PathBuf {
        self.directory.join(format!("{name}.json"))
    }

    fn log_file(&self, name: &str) -> PathBuf {
        self.directory.join(format!("{name}.log"))
    }
}

/// Starts a named ACP server in the background and returns its URL.
pub async fn start(options: StartOptions) -> Result<String> {
    validate_name(&options.name)?;
    let executable = std::env::current_exe().context("failed to locate acp-agent executable")?;
    start_with(options, ServerPaths::discover()?, executable, START_TIMEOUT).await
}

async fn start_with(
    options: StartOptions,
    paths: ServerPaths,
    executable: PathBuf,
    start_timeout: Duration,
) -> Result<String> {
    validate_name(&options.name)?;
    let path = paths.state_file(&options.name);
    if tokio::fs::try_exists(&path).await.unwrap_or(false) {
        if let Ok(existing) = read_json::<ServerFile>(&path).await
            && server_is_alive(&existing).await
        {
            bail!(
                "server \"{}\" is already running at {}",
                options.name,
                public_url(&existing.listen_host, existing.port)?
            );
        }
        let _ = tokio::fs::remove_file(&path).await;
    }

    let log_path = paths.log_file(&options.name);
    let log = open_private_file(&log_path, true)?;
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
        .env(SERVER_STATE_DIR_ENV, &paths.directory)
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
                && state.name == options.name
                && state.version == SERVER_PROTOCOL_VERSION
                && server_is_alive(&state).await
            {
                return Ok(state);
            }
            sleep(Duration::from_millis(50)).await;
        }
    };
    let state = match timeout(start_timeout, ready).await {
        Ok(Ok(state)) => state,
        Ok(Err(error)) => {
            cleanup_failed_start(&mut child, &path)
                .await
                .with_context(|| format!("failed to clean up after startup error: {error:#}"))?;
            return Err(error);
        }
        Err(_) => {
            cleanup_failed_start(&mut child, &path)
                .await
                .context("failed to clean up after server startup timeout")?;
            bail!(
                "timed out waiting for server to start; inspect {}",
                log_path.display()
            );
        }
    };
    Ok(format!(
        "started server \"{}\" at {}",
        state.name,
        public_url(&state.listen_host, state.port)?
    ))
}

/// Stops a named ACP server.
pub async fn stop(name: &str) -> Result<String> {
    validate_name(name)?;
    let path = ServerPaths::discover()?.state_file(name);
    let state: ServerFile = read_json(&path)
        .await
        .with_context(|| format!("server \"{name}\" is not running"))?;
    if state.name != name || !server_is_alive(&state).await {
        bail!("server \"{name}\" is not running");
    }
    let response = reqwest::Client::new()
        .post(format!("{}/api/shutdown", state.control_url))
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
        .post(format!("{}/api/agents", state.control_url))
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
        public_url(&state.listen_host, state.port)?
    ))
}

/// Removes an agent router from a named ACP server.
pub async fn unregister(agent_id: &str, name: &str) -> Result<String> {
    validate_name(name)?;
    let state = load_live_server(name).await?;
    let response = reqwest::Client::new()
        .delete(format!("{}/api/agents", state.control_url))
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
    run_with(name, host, port, ServerPaths::discover()?, SHUTDOWN_GRACE).await
}

async fn run_with(
    name: String,
    host: String,
    port: u16,
    paths: ServerPaths,
    shutdown_grace: Duration,
) -> Result<()> {
    validate_name(&name)?;
    let listener = TcpListener::bind((host.as_str(), port))
        .await
        .with_context(|| format!("failed to bind named server on {host}:{port}"))?;
    let address = listener
        .local_addr()
        .context("failed to read named server address")?;
    if !is_loopback_host(&host) {
        eprintln!(
            "warning: unauthenticated server management endpoints are reachable on a non-loopback interface"
        );
    }
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let state = ServerState {
        server_name: name.clone(),
        agents: Arc::default(),
        shutdown: shutdown_tx,
    };
    let router = server_router(state.clone());
    let control_url = control_url(&host, address.port())?;
    let file = ServerFile {
        name: name.clone(),
        listen_host: host,
        port: address.port(),
        control_url,
        pid: std::process::id(),
        version: SERVER_PROTOCOL_VERSION.to_string(),
    };
    let state_path = paths.state_file(&name);
    write_private_json_atomic(&state_path, &file)?;
    eprintln!(
        "Serving named ACP server \"{name}\" at {}",
        public_url(&file.listen_host, file.port)?
    );

    let result = serve_with_shutdown(listener, router, shutdown_rx, shutdown_grace).await;
    let cleanup = match tokio::fs::remove_file(&state_path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to remove server state {}", state_path.display())),
    };
    result.and(cleanup)
}

async fn serve_with_shutdown(
    listener: TcpListener,
    router: Router,
    shutdown_rx: watch::Receiver<bool>,
    shutdown_grace: Duration,
) -> Result<()> {
    let mut server_shutdown = shutdown_rx.clone();
    let mut supervisor_shutdown = shutdown_rx;
    let server = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            wait_for_shutdown(&mut server_shutdown).await;
        })
        .into_future();
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result.context("named ACP server failed"),
        () = wait_for_shutdown(&mut supervisor_shutdown) => {
            match timeout(shutdown_grace, &mut server).await {
                Ok(result) => result.context("named ACP server failed"),
                Err(_) => Ok(()),
            }
        }
    }
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

async fn server_status(State(state): State<ServerState>) -> Json<ServerStatus> {
    Json(ServerStatus {
        name: state.server_name,
        pid: std::process::id(),
        version: SERVER_PROTOCOL_VERSION.to_string(),
    })
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
    state.shutdown.send_replace(true);
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
    let state: ServerFile = read_json(&ServerPaths::discover()?.state_file(name))
        .await
        .with_context(|| format!("server \"{name}\" is not running"))?;
    if !server_is_alive(&state).await {
        bail!("server \"{name}\" is not running");
    }
    Ok(state)
}

async fn server_is_alive(state: &ServerFile) -> bool {
    let Ok(response) = reqwest::Client::new()
        .get(format!("{}/api/status", state.control_url))
        .timeout(Duration::from_millis(500))
        .send()
        .await
    else {
        return false;
    };
    if !response.status().is_success() {
        return false;
    }
    response.json::<ServerStatus>().await.is_ok_and(|status| {
        status.name == state.name
            && status.version == state.version
            && status.version == SERVER_PROTOCOL_VERSION
    })
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

fn ensure_private_directory(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("failed to create server state directory {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).with_context(
            || format!("failed to secure server state directory {}", path.display()),
        )?;
    }
    // Windows inherits the current user's ACL from the platform cache root;
    // Unix modes do not have an ACL-equivalent meaning there.
    Ok(())
}

fn open_private_file(path: &Path, append: bool) -> Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.create(true).write(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("failed to open private file {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to secure private file {}", path.display()))?;
    }
    // On Windows the newly created file inherits the private cache ACL.
    Ok(file)
}

async fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("failed to parse {}", path.display()))
}

fn write_private_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).context("failed to serialize server state")?;
    let parent = path
        .parent()
        .context("server state path has no parent directory")?;
    ensure_private_directory(parent)?;
    let file_name = path
        .file_name()
        .context("server state path has no file name")?
        .to_string_lossy();
    let (temporary, mut file) = loop {
        let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{file_name}.{}.{}.tmp",
            std::process::id(),
            counter
        ));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temporary) {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to create {}", temporary.display()));
            }
        }
    };

    let result = (|| -> Result<()> {
        file.write_all(&bytes)
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", temporary.display()))?;
        drop(file);
        std::fs::rename(&temporary, path).with_context(|| {
            format!(
                "failed to replace server state {} with {}",
                path.display(),
                temporary.display()
            )
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn control_url(listen_host: &str, port: u16) -> Result<String> {
    let host = match listen_host {
        "0.0.0.0" => "127.0.0.1",
        "::" | "[::]" => "::1",
        other => other,
    };
    http_url(host, port)
}

fn public_url(host: &str, port: u16) -> Result<String> {
    http_url(host, port)
}

fn http_url(host: &str, port: u16) -> Result<String> {
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let mut url = reqwest::Url::parse("http://localhost")?;
    if let Ok(address) = host.parse::<std::net::IpAddr>() {
        url.set_ip_host(address)
            .map_err(|_| anyhow::anyhow!("invalid server host {host:?}"))?;
    } else {
        url.set_host(Some(host))
            .map_err(|_| anyhow::anyhow!("invalid server host {host:?}"))?;
    }
    url.set_port(Some(port))
        .map_err(|_| anyhow::anyhow!("invalid server port {port}"))?;
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn is_loopback_host(host: &str) -> bool {
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

async fn wait_for_shutdown(receiver: &mut watch::Receiver<bool>) {
    while !*receiver.borrow() {
        if receiver.changed().await.is_err() {
            break;
        }
    }
}

async fn cleanup_failed_start(child: &mut tokio::process::Child, state_path: &Path) -> Result<()> {
    let _ = child.start_kill();
    child
        .wait()
        .await
        .context("failed to wait for server process during cleanup")?;
    match tokio::fs::remove_file(state_path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to remove server state {}", state_path.display())),
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
        ServerState {
            server_name: "test".to_string(),
            agents: Arc::default(),
            shutdown,
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
    async fn readiness_requires_matching_name_and_version() {
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
